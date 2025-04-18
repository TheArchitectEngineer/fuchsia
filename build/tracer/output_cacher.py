#!/usr/bin/env fuchsia-vendored-python
# Copyright 2021 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Wraps a command so that its outputs are timestamp-fresh only if their contents change.

Every declared output is renamed with a temporary suffix in the command.
If the command succeeds, the temporary file is moved over the original declared
output if the output did not already exist or the contents are different.
This conditional move is done for every declared output that appears in the
arguments list.

This is intended to be used in build systems like Ninja that support `restat`:
treating unchanged outputs as up-to-date, which has the potential to prune
the action graph on-the-fly.

Assumptions:
  Output files can be whole shell tokens in the command's arguments.
    We also support filenames as lexical substrings in tokens like
    "--flag=out1,out2" or just "out1,out2".

  If x is a writeable path (output), then x.any_suffix is also writeable.

  If x is a writeable path (output), then dirname(x) is also writeable.

  Command being wrapped does not change behavior with the name of its output
  arguments.

If any of the above assumptions do not hold, then we recommend --disable
wrapping.
"""

import argparse
import dataclasses
import filecmp
import os
import shutil
import subprocess
import sys
import time
from pathlib import Path
from typing import Any, Callable, Dict, Iterable, Sequence, Tuple

_SCRIPT_BASENAME = Path(__file__).name
_SCRIPT_DIR = Path(__file__).parent
_PROJECT_ROOT = _SCRIPT_DIR.resolve().parent.parent
_PROJECT_ROOT_REL = os.path.relpath(
    _PROJECT_ROOT, start=Path(os.curdir).absolute()
)
_DETAIL_DIFF_SCRIPT = Path("build/rbe/detail-diff.sh")


def msg(text: str):
    print(f"[{_SCRIPT_BASENAME}] {text}")


def _partition(
    iterable: Iterable[Any], predicate: Callable[[Any], bool]
) -> Tuple[Sequence[Any], Sequence[Any]]:
    """Splits sequence into two sequences based on predicate function."""
    trues = []
    falses = []
    for item in iterable:
        if predicate(item):
            trues.append(item)
        else:
            falses.append(item)
    return trues, falses


def files_match(file1: Path, file2: Path):
    """Compares two files, returns True if they both exist and match."""
    # filecmp.cmp does not invoke any subprocesses.
    return filecmp.cmp(file1, file2, shallow=False)


def ensure_file_exists(path: Path):
    """Assert that a file exists, or wait for it to appear.

    It has been shown that some fault tolerance is needed
    regarding expecting files to be produced by a subprocess.

    Args:
      path: path to file that is expected to exist.

    Raises:
      FileNotFoundError if path does not exist, even after waiting.
    """
    for delay in (3, 6, 15):
        if Path.exists():
            return

        # This branch should be highly unlikely, so it is allowed to be slow.
        # Either the original command failed to produce this file, or something
        # could be wrong with file system synchronization or delays.
        # Flush writes, sleep, try again.
        msg(
            f"Expected output file not found: {path} (Retrying after {delay}s ...)"
        )
        os.sync()
        time.sleep(delay)

    raise FileNotFoundError(
        f"[{_SCRIPT_BASENAME}] *** Expected output file not found: {path}"
    )


def detail_diff(left: Path, right: Path):
    """Richer difference analysis."""
    print(f"  {left} vs. {right}")
    subprocess.call(
        [
            _PROJECT_ROOT_REL / _DETAIL_DIFF_SCRIPT,
            # denote first/second for checking repeatability
            "-l=first",
            "-r=second",
            str(left),
            str(right),
        ]
    )


def remove_if_exists(path: Path):
    if path.exists():
        path.unlink()


def execute_main_command(*args, **kwargs) -> int:
    """Forwards to subprocess.call, for ease of mocking."""
    return subprocess.call(*args, **kwargs)


# TODO: de-dupe this from cl_utils after moving this source file
def copy_preserve_subpath(src: Path, dest_dir: Path):
    """Like copy(), but preserves the relative path of src in the destination."""
    assert (
        not src.is_absolute()
    ), f"source file to be copied should be relative, but got: {src}"
    dest_subdir = dest_dir / src.parent
    dest_subdir.mkdir(parents=True, exist_ok=True)
    shutil.copy2(src, dest_subdir)


def retry_file_op_once_with_delay(
    fileop: Callable[[], Any], failmsg: str, delay: int
):
    """Insanity is doing the same thing and expecting a different result."""
    try:
        fileop()
    except FileNotFoundError:
        # one-time retry
        msg(f"{failmsg}  (Retrying once after {delay}s.)")
        time.sleep(delay)
        fileop()
        # If this fails again, exception will be raised.


def move_if_different(src: Path, dest: Path, verbose: bool = False) -> bool:
    """Moves src -> dest if their contents differ.

    Args:
      src: source path
      dest: destination path
      verbose: if True, print whether a move actually happened.

    Returns:
      True if move occurred,
      False if the destination already matches source.
    """
    ensure_file_exists(src)
    if not dest.exists() or not files_match(dest, src):
        if verbose:
            msg(f"  === Updated: {dest}")
        src.rename(dest)
        return True
    else:
        if verbose:
            msg(f"  === Cached: {dest}")
        src.unlink()
        return False


@dataclasses.dataclass
class TempFileTransform(object):
    """Represents a file name transform.

     At least temp_dir or suffix or basename_prefix must be non-blank.

    temp_dir: Write temporary files in here.
      If blank, paths are relative to working directory.
    suffix: Add this suffix to temporary files, e.g. ".tmp".
    basename_prefix: Add this prefix to the basename of the path.
      This can be a good choice over suffix when the underlying tool behavior
      is sensitive to the output file extension.
      Example: "foo/bar.txt", with prefix="tmp-" -> foo/tmp-bar.txt
    """

    temp_dir: Path = None
    suffix: str = ""
    basename_prefix: str = ""

    @property
    def valid(self):
        return self.temp_dir or self.suffix or self.basename_prefix

    def transform(self, path: Path) -> Path:
        subpath = path.parent / (self.basename_prefix + path.name + self.suffix)
        return self.temp_dir / subpath if self.temp_dir else subpath


def split_transform_join(
    token: str, sep: str, transform: Callable[[str], str]
) -> str:
    return sep.join(transform(x) for x in token.split(sep))


def lexically_rewrite_token(token: str, transform: Callable[[str], str]) -> str:
    """Lexically replaces substrings between delimiters.

    This is useful for transforming substrings of text.

    This can transform "--foo=bar,baz" into
    f("--foo") + "=" + f("bar") + "," + f("baz")

    Args:
      token: text to transform, like a shell token.
      transform: text transformation.

    Returns:
      text with substrings transformed.
    """

    def inner_transform(text: str) -> str:
        return split_transform_join(text, ",", transform)

    return split_transform_join(token, "=", inner_transform)


class OutputSubstitution(object):
    def __init__(self, spec: str):
        """Constructs an OutputSubstitution.

        Args:
          spec: a string that is either:
            * a filename to substitute
            * a specification of the form 'substitute_after:OPTION:FILENAME'
              where OPTION is a flag to match, like '--output',
              and FILENAME is the output file name to substitute.
            File names may not contain the characters: =:,
            See help for the --output option.
        """
        if spec.startswith("substitute_after:"):
            tokens = spec.split(":")
            if len(tokens) != 3:
                raise ValueError(
                    f"Expecting a substitution specification FILENAME or "
                    + f"substitute_after:OPTION:FILENAME, but got {spec}."
                )
            self._match_previous_option = tokens[1]
            self._output_name = tokens[2]
        else:
            # if blank, this will not be used for matching
            self._match_previous_option = ""
            self._output_name = spec

    @property
    def match_previous_option(self) -> str:
        return self._match_previous_option

    @property
    def output_name(self) -> str:
        return self._output_name


@dataclasses.dataclass
class Action(object):
    """Represents a set of parameters of a single build action."""

    command: Sequence[str] = dataclasses.field(default_factory=list)
    substitutions: Dict[str, str] = dataclasses.field(
        default_factory=dict
    )  # FrozenDict
    label: str = ""

    def substitute_command(
        self, tempfile_transform: TempFileTransform
    ) -> Tuple[Sequence[str], Dict[Path, Path]]:
        # renamed_outputs: keys: original file names, values: transformed temporary file names
        renamed_outputs: Dict[Path, Path] = {}

        def replace_output_filename(arg: str, prev_opt: str) -> Path:
            arg_path = Path(arg)
            if arg in self.substitutions:
                match_previous = self.substitutions[arg]
                # Some output filenames requires the previous option to match.
                if match_previous != "" and prev_opt != match_previous:
                    return arg_path
                new_arg = tempfile_transform.transform(arg_path)
                if arg_path != new_arg:
                    renamed_outputs[arg_path] = new_arg
                return new_arg
            else:
                return arg_path

        substituted_command = []
        # Subprocess calls do not work for commands that start with VAR=VALUE
        # environment variables, which is remedied by prefixing with 'env'.
        if self.command and "=" in self.command[0]:
            substituted_command += ["/usr/bin/env"]

        substituted_command += [
            lexically_rewrite_token(
                tok, lambda x: str(replace_output_filename(x, prev_opt))
            )
            for prev_opt, tok in zip([""] + self.command[:-1], self.command)
        ]

        return substituted_command, renamed_outputs

    def run_cached(
        self,
        tempfile_transform: TempFileTransform,
        verbose: bool = False,
        dry_run: bool = False,
    ) -> int:
        """Runs a modified command and conditionally moves outputs in-place.

        Args:
          tempfile_transform: describes transformation to temporary file name.
          verbose: If True, print substituted command before running it.
          dry_run: If True, print substituted command and stop.
        """

        # renamed_outputs: keys: original file names, values: transformed temporary file names
        substituted_command, renamed_outputs = self.substitute_command(
            tempfile_transform
        )

        if verbose or dry_run:
            for orig, renamed in renamed_outputs.items():
                msg(f"=== renamed: {orig} -> {renamed}")
            cmd_str = " ".join(substituted_command)
            msg(f"=== substituted command: {cmd_str}")

        if dry_run:
            return 0

        # mkdir when needed.
        if tempfile_transform.temp_dir:
            for new_arg in renamed_outputs.values():
                new_arg.parent.mkdir(parents=True, exist_ok=True)

        # Run the modified command.
        retval = execute_main_command(substituted_command)

        if retval != 0:
            # Option: clean-up .tmp files or leave them for inspection
            return retval

        # Otherwise command succeeded, so conditionally move outputs in-place.
        # TODO(fangism): This loop could be parallelized.
        move_err = False
        for orig_out, temp_out in renamed_outputs.items():
            try:
                retry_file_op_once_with_delay(
                    lambda: move_if_different(
                        src=temp_out, dest=orig_out, verbose=verbose
                    ),
                    f"Failed to update {temp_out} -> {orig_out}.",
                    5,
                )
            except FileNotFoundError as e:
                msg(str(e))
                move_err = True

        if move_err:
            msg("  *** Aborting due to previous error.")
            return 1

        if verbose:
            unrenamed_outputs = set(self.substitutions.keys()) - set(
                renamed_outputs.keys()
            )
            if unrenamed_outputs:
                # Having un-renamed outputs is not an error, but rather an indicator
                # of a potentially missed opportunity to cache unchanged outputs.
                unrenamed_formatted = " ".join(unrenamed_outputs)
                msg(f"  === Un-renamed outputs: {unrenamed_formatted}")

        return 0

    def run_repeatedly_and_compare_outputs(
        self,
        tempfile_transform: TempFileTransform,
        diff_action: Callable[[Path, Path], Any],
        max_attempts: int = 1,
        verbose: bool = False,
    ) -> int:
        """Runs a command N times, copying declared outputs in between.

        Compare both sets of outputs, and error out if any differ.
        The advantage of this variant over others is that it eliminates
        output-path sensitivities by running the *same* command N times.
        One possible disadvantage is that this may expose behavioral
        differences due to the non/pre-existence of outputs ahead of
        running the command.

        Args:
          tempfile_transform: used to rename backup copies of outputs.
          diff_action: action to run on files with differences.
          max_attempts: number of times to re-run and compare (after the first).
          verbose: if True, print more diagnostics.

        Returns:
          exit code 0 on command success and outputs match, else nonzero.
        """

        # Run the command the first time.
        retval = execute_main_command(self.command)

        # If the command failed, skip re-running.
        if retval != 0:
            return retval

        # Backup a copy of all declared outputs.
        renamed_outputs: Dict[Path, Path] = {}
        for out in self.substitutions:
            # TODO(fangism): what do we do about symlinks?
            # TODO(fangism): An output *directory* is unexpected, coming from GN,
            # but has been observed.  For now skip it.
            out_path = Path(out)
            if out_path.is_file():
                renamed_outputs[out_path] = tempfile_transform.transform(
                    out_path
                )
            # A nonexistent output would be caught by action_tracer.py.

        for out, backup in renamed_outputs.items():
            if tempfile_transform.temp_dir:
                backup.parent.mkdir(parents=True, exist_ok=True)
            # preserve metadata such as timestamp
            shutil.copy2(out, backup, follow_symlinks=False)

        # Delaying re-run by 1s greatly increases the chances of catching
        # textual clock time differences, like __TIME__ in C-preprocessing.
        time.sleep(1)

        for i in range(max_attempts):
            # Expect re-running command to overwrite existing outputs.
            rerun_retval = execute_main_command(self.command)
            if rerun_retval != 0:
                msg(
                    f"Re-run attempt {i} of command {self.command} failed, where previous runs succeeded!?"
                )
                return rerun_retval

            verify_status = verify_files_match(
                fileset=renamed_outputs,
                label=self.label,
                renamed=False,
                diff_action=diff_action,
                # re-use for multiple attempts, clean up before returning.
                keep_backups=True,
            )
            if verify_status != 0:
                msg(
                    f"Inconsistent output found on re-run attempt {i}/{max_attempts}."
                )
                # Keep around mismatched outputs.
                return verify_status

        # Reaching this point means all re-runs produced identical outputs.
        # On success, remove the backup copies to save space.
        for backup in renamed_outputs.values():
            remove_if_exists(backup)
        return 0

    def run_twice_with_substitution_and_compare_outputs(
        self,
        tempfile_transform: TempFileTransform,
        diff_action: Callable[[Path, Path], Any],
        verbose: bool = False,
    ) -> int:
        """Runs a command twice, the second time with renamed outputs, and compares.

        Caveat: If the contents if the outputs are sensitive to the names of the
        outputs, this will find too many differences.

        Args:
          tempfile_transform: used to rename backup copies of outputs.
          diff_action: action to run on files with differences.
          verbose: if True, print more diagnostics.

        Returns:
          exit code 0 on command success and outputs match, else nonzero.
        """

        # renamed_outputs: keys: original file names, values: transformed temporary file names
        substituted_command, renamed_outputs = self.substitute_command(
            tempfile_transform
        )

        if verbose:
            cmd_str = " ".join(substituted_command)
            print(f"=== substituted command: {cmd_str}")

        # mkdir when needed.
        if tempfile_transform.temp_dir:
            for new_arg in renamed_outputs.values():
                new_arg.parent.mkdir(parents=True, exist_ok=True)

        # Run the original command.
        retval = execute_main_command(self.command)

        # If the command failed, skip re-running.
        if retval != 0:
            return retval

        # Otherwise command succeeded, re-run with different output locations.
        rerun_retval = execute_main_command(substituted_command)
        if rerun_retval != 0:
            print(
                f"Re-run failed with substituted outputs of target [{self.label}]: {substituted_command}"
            )
            return rerun_retval

        return verify_files_match(
            fileset=renamed_outputs,
            diff_action=diff_action,
            label=self.label,
            renamed=True,
            keep_backups=False,
        )


def verify_files_match(
    fileset: Dict[Path, Path],
    diff_action: Callable[[Path, Path], Any],
    label: str,
    renamed: bool,
    keep_backups: bool,
) -> int:
    """Compare outputs and report differences.

    Remove matching copies.

    Args:
      fileset: {file: backup} key-value pairs of files to compare. Backup files
        that match are removed to save space, while the .keys() files are kept.
      diff_action: action to run on files with differences, e.g. more
        detailed diff analysis, copy files.
      label: An identifier for the action that was run, for diagnostics.
      renamed: True if files to compare were renamed.
      keep_backups: If False, delete matching copies after comparison.

    Returns:
      exit code 0 if all files matched, else 1.
    """
    matching_files, different_files = _partition(
        fileset.items(),
        # If either file is missing, this will fail, which indicates that
        # something is not working as expected.
        lambda pair: files_match(pair[0], pair[1]),
    )

    if not keep_backups:
        # Remove any files that matched to save space.
        for _, temp_out in matching_files:
            temp_out.unlink()

    if different_files:
        label_text = ""
        if label:
            label_text = " for target [{label}]"

        renamed_text = ""
        if renamed:
            renamed_text = " with renamed outputs"

        print(
            f"Repeating command{label_text}{renamed_text} produces different results:"
        )

        for orig, temp in different_files:
            diff_action(orig, temp)

        # Note: Even though the original command succeeded, forcing this to
        # fail may influence tools that examine the freshness of outputs
        # relative to the last succeeded command.
        return 1

    return 0


def _main_arg_parser() -> argparse.ArgumentParser:
    """Construct the argument parser, called by main()."""
    parser = argparse.ArgumentParser(
        description="Wraps a GN action to preserve unchanged outputs",
        argument_default=[],
    )
    # label is only used for diagnostics
    parser.add_argument(
        "--label",
        type=str,
        default="",
        help="The wrapped target's label",
    )
    parser.add_argument(
        "--outputs",
        nargs="*",
        help="An action's declared outputs.  "
        + "When an element is a plain file name, all occurrences of that file name "
        + "will be substituted in the command that writes temporary outputs.  "
        + "When an element has the form 'substitute_after:OPTION:FILENAME', "
        + "only occurrences of FILENAME found in the option argument of OPTION "
        + "will be substituted (examples: OPTION=-o or OPTION=--out).  "
        + "The latter form is recommended when an output filename can occur in "
        + "multiple locations in a command line.  "
        + "File names must not contain =,: characters.",
    )
    parser.add_argument(
        "--temp-suffix",
        type=str,
        default="",
        help="Suffix to use for temporary outputs",
    )
    parser.add_argument(
        "--temp-prefix",
        type=str,
        default="tmp-",
        help="Basename prefix to use for temporary outputs",
    )
    parser.add_argument(
        "--temp-dir",
        type=Path,
        default=None,
        help="Temporary directory for writing, can be relative to working directory or absolute.",
    )
    parser.add_argument(
        "--verbose",
        action="store_true",
        default=False,
        help="Print information about which outputs were renamed/cached.",
    )
    parser.add_argument(
        "--dry-run",
        action="store_true",
        default=False,
        help="Show transformed command and exit.",
    )
    parser.add_argument(
        "--disable",
        action="store_false",
        dest="enable",
        default=True,
        help="If disabled, run the original command as-is.",
    )
    parser.add_argument(
        "--check-repeatability",
        action="store_true",
        default=False,
        help="Check for repeatability: run the command twice, with different outputs, and compare.",
    )
    parser.add_argument(
        "--max-attempts",
        type=int,
        default=1,
        help="For --check-repeatability, re-run and compare this many times.",
    )
    parser.add_argument(
        "--rename-outputs",
        action="store_true",
        default=False,
        help="When checking for repeatability: rename command-line outputs on the second run.",
    )
    parser.add_argument(
        "--miscomparison-export-dir",
        type=Path,
        default=None,
        metavar="DIR",
        help="When using --check-repeatability, save unexpectedly different artifacts to this directory, preserving relative path under the working directory.",
    ),

    # Positional args are the command and arguments to run.
    parser.add_argument("command", nargs="*", help="The command to run")
    return parser


_MAIN_ARG_PARSER = _main_arg_parser()


def default_diff_action(
    left: Path, right: Path, miscomparison_export_dir: Path = None
):
    # Run detailed analysis, using various binary dumps.
    detail_diff(left, right)

    # Keep around different outputs for analysis.
    if miscomparison_export_dir:
        copy_preserve_subpath(left, miscomparison_export_dir)
        copy_preserve_subpath(right, miscomparison_export_dir)


def main():
    args = _MAIN_ARG_PARSER.parse_args()

    tempfile_transform = TempFileTransform(
        temp_dir=args.temp_dir,
        suffix=args.temp_suffix,
        basename_prefix=args.temp_prefix,
    )
    if not tempfile_transform.valid:
        raise ValueError(
            "Need either --temp-dir or --temp-suffix, but both are missing."
        )

    wrap = args.enable
    # Decided whether or not to wrap the action script.
    ignored_scripts = {
        # If the action is only copying or linking, don't bother wrapping.
        "ln",
        "cp",  # TODO: Could conditionally copy if different.
        "rsync",
    }
    script = Path(args.command[0])
    if script.name in ignored_scripts:
        wrap = False

    # If disabled, run the original command as-is.
    if not wrap:
        return execute_main_command(args.command)

    try:
        substitutions = [OutputSubstitution(x) for x in args.outputs]
    except ValueError as e:
        print(str(e))
        return 1

    substitutions_dict = {
        x.output_name: x.match_previous_option for x in substitutions
    }

    # Run a modified command that can leave unchanged outputs untouched.
    action = Action(
        command=args.command,
        substitutions=substitutions_dict,
        label=args.label,
    )

    # Run one of the following modes:
    # check_repeatability: run the command twice, and compare the outputs.
    # [default]: redirect outputs to temporary locations, and move them
    #   in-place to their original locations if contents have not changed.

    if args.check_repeatability:

        def _diff_action(left: Path, right: Path):
            default_diff_action(left, right, args.miscomparison_export_dir)

        if args.rename_outputs:
            # This check variant will find path-sensitive outputs
            # (for example, outputs that contain their own path),
            # and nondeterminstic outputs.
            return action.run_twice_with_substitution_and_compare_outputs(
                tempfile_transform=tempfile_transform,
                diff_action=_diff_action,
                verbose=args.verbose,
            )
        else:
            # This check will only find nondeterministic outputs.
            # For example, those affected by the current time.
            return action.run_repeatedly_and_compare_outputs(
                tempfile_transform=tempfile_transform,
                diff_action=_diff_action,
                max_attempts=args.max_attempts,
                verbose=args.verbose,
            )

    return action.run_cached(
        tempfile_transform=tempfile_transform,
        verbose=args.verbose,
        dry_run=args.dry_run,
    )


if __name__ == "__main__":
    sys.exit(main())
