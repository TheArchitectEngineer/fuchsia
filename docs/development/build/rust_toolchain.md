# Build a custom Rust toolchain for Fuchsia

This guide explains how to build a Rust compiler for use with the Fuchsia. This
is useful if you need to build Fuchsia with a patched compiler, or a compiler
built with custom options. Building a custom Rust toolchain is not always
necessary for building Fuchsia with a different version of Rust; see
[Build Fuchsia with a custom Rust toolchain](/docs/development/build/fuchsia_custom_rust.md)
for details.

## Prerequisites

Prior to building a custom Rust toolchain for Fuchsia, you need to do the following:

1. If you haven't already, clone the Rust source. The
   [Guide to Rustc Development] is a good resource to reference whenever you're
   working on the compiler.

   ```posix-terminal
   DEV_ROOT={{ '<var>' }}DEV_ROOT{{ '</var> '}} # parent of your Rust directory
   git clone --recurse-submodules https://github.com/rust-lang/rust.git $DEV_ROOT/rust
   ```

1. Run the following command to install cmake and ninja:

   ```posix-terminal
   sudo apt-get install cmake ninja-build
   ```

1. Run the following command to obtain the infra sources:

   ```posix-terminal
   DEV_ROOT={{ '<var>' }}DEV_ROOT{{ '</var> '}} # parent of your Rust directory

   mkdir -p $DEV_ROOT/infra && \
   ( \
     builtin cd $DEV_ROOT/infra && \
     jiri init && \
     jiri import -overwrite -name=fuchsia/manifest infra \
         https://fuchsia.googlesource.com/manifest && \
     jiri update \
   )
   ```

   Note: Running `jiri update` from the `infra` directory ensures that you
   have the most recent configurations and tools.

1. Run the following command to use `cipd` to get a Fuchsia core IDK, a Linux
   sysroot, a recent version of clang, and the correct beta compiler for
   building Fuchsia's Rust toolchain:

   ```posix-terminal
   DEV_ROOT={{ '<var>' }}DEV_ROOT{{ '</var>' }}
   HOST_TRIPLE={{ '<var>' }}x86_64-unknown-linux-gnu{{ '</var>' }}
   cat << "EOF" > cipd.ensure
   @Subdir sdk
   fuchsia/sdk/core/${platform} latest
   @Subdir sysroot/linux
   fuchsia/third_party/sysroot/linux git_revision:db18eec0b4f14b6b16174aa2b91e016663157376
   @Subdir sysroot/focal
   fuchsia/third_party/sysroot/focal latest
   @Subdir clang
   fuchsia/third_party/clang/${platform} integration
   EOF

   STAGE0_DATE=$(sed -nr 's/^compiler_date=(.*)/\1/p' ${DEV_ROOT}/rust/src/stage0)
   STAGE0_VERSION=$(sed -nr 's/^compiler_version=(.*)/\1/p' ${DEV_ROOT}/rust/src/stage0)
   STAGE0_COMMIT_HASH=$( \
     curl -s "https://static.rust-lang.org/dist/${STAGE0_DATE}/channel-rust-${STAGE0_VERSION}.toml" \
     | python3 -c 'import tomllib, sys; print(tomllib.load(sys.stdin.buffer)["pkg"]["rust"]["git_commit_hash"])')
   echo "@Subdir stage0" >> cipd.ensure
   echo "fuchsia/third_party/rust/host/\${platform} rust_revision:${STAGE0_COMMIT_HASH}" >> cipd.ensure
   echo "fuchsia/third_party/rust/target/${HOST_TRIPLE} rust_revision:${STAGE0_COMMIT_HASH}" >> cipd.ensure
   $DEV_ROOT/infra/fuchsia/prebuilt/tools/cipd ensure --root $DEV_ROOT --ensure-file cipd.ensure
   ```

   Note: these versions are not pinned, so every time you run the `cipd ensure`
   command, you will get an updated version. As of writing, however, this
   matches the recipe behavior.

   Downloading the Fuchsia-built stage0 compiler is optional, but useful for
   recreating builds in CI. If the stage0 is not available you may instruct
   the Rust build to download and use the upstream stage0 compiler by omitting
   those lines from your `cipd.ensure` file and removing the `--stage0`
   arguments to `generate_config.py` below.

[Guide to Rustc Development]: https://rustc-dev-guide.rust-lang.org/building/how-to-build-and-run.html

## Configure Rust for Fuchsia

1. Change into your Rust directory.
1. Run the following command to generate a configuration for the Rust toolchain:

   ```posix-terminal
   DEV_ROOT={{ '<var>' }}DEV_ROOT{{ '</var>' }}

   $DEV_ROOT/infra/fuchsia/prebuilt/tools/vpython3 \
     $DEV_ROOT/infra/fuchsia/recipes/recipes/rust_toolchain.resources/generate_config.py \
       config_toml \
       --clang-prefix=$DEV_ROOT/clang \
       --host-sysroot=$DEV_ROOT/sysroot/linux \
       --stage0=$DEV_ROOT/stage0 \
       --targets=aarch64-unknown-linux-gnu,x86_64-unknown-linux-gnu,thumbv6m-none-eabi,thumbv7m-none-eabi,riscv32imc-unknown-none-elf,riscv64gc-unknown-linux-gnu \
       --prefix=$(pwd)/install/fuchsia-rust \
      | tee fuchsia-config.toml

   $DEV_ROOT/infra/fuchsia/prebuilt/tools/vpython3 \
       $DEV_ROOT/infra/fuchsia/recipes/recipes/rust_toolchain.resources/generate_config.py \
         environment \
         --eval \
         --clang-prefix=$DEV_ROOT/clang \
         --sdk-dir=$DEV_ROOT/sdk \
         --stage0=$DEV_ROOT/stage0 \
         --targets=aarch64-unknown-linux-gnu,x86_64-unknown-linux-gnu,thumbv6m-none-eabi,thumbv7m-none-eabi,riscv32imc-unknown-none-elf,riscv64gc-unknown-linux-gnu \
         --linux-sysroot=$DEV_ROOT/sysroot/linux \
         --linux-riscv64-sysroot=$DEV_ROOT/sysroot/focal \
      | tee fuchsia-env.sh
   ```

1. (Optional) Run the following command to tell git to ignore the generated files:

   ```posix-terminal
   echo fuchsia-config.toml >> .git/info/exclude

   echo fuchsia-env.sh >> .git/info/exclude
   ```

1. (Optional) Customize `fuchsia-config.toml`.

## Build and install Rust

1. Change into your Rust source directory.
1. Run the following command to build and install Rust plus the Fuchsia runtimes spec:

   ```posix-terminal
   DEV_ROOT={{ '<var>' }}DEV_ROOT{{ '</var>' }}

   rm -rf install/fuchsia-rust
   mkdir -p install/fuchsia-rust

   # Copy and paste the following subshell to build and install Rust, as needed.
   # The subshell avoids polluting your environment with fuchsia-specific rust settings.
   ( source fuchsia-env.sh && ./x.py install --config fuchsia-config.toml \
     --skip-stage0-validation ) && \
   rm -rf install/fuchsia-rust/lib/.build-id && \
   $DEV_ROOT/infra/fuchsia/prebuilt/tools/vpython3 \
     $DEV_ROOT/infra/fuchsia/recipes/recipes/rust_toolchain.resources/generate_config.py \
       runtime \
     | $DEV_ROOT/infra/fuchsia/prebuilt/tools/vpython3 \
         $DEV_ROOT/infra/fuchsia/recipes/recipe_modules/toolchain/resources/runtimes.py \
           --dir install/fuchsia-rust/lib \
           --dist dist \
           --readelf fuchsia-build/host/llvm/bin/llvm-readelf \
           --objcopy fuchsia-build/host/llvm/bin/llvm-objcopy \
     > install/fuchsia-rust/lib/runtime.json
   ```

### Build only (optional)

If you want to skip the install step, for instance during development of Rust
itself, you can do so with the following command.

```posix-terminal
( source fuchsia-env.sh && ./x.py build --config fuchsia-config.toml \
  --skip-stage0-validation )
```

### Troubleshooting

If you are getting build errors, try deleting the Rust build directory:

```posix-terminal
rm -rf fuchsia-build
```

Then re-run the command to build Rust.

## Building Fuchsia with a custom Rust toolchain

With a newly compiled custom Rust toolchain, you're ready to use it to build
Fuchsia. Directions on how to do so are available in a [dedicated guide].

[dedicated guide]: /docs/development/build/fuchsia_custom_rust.md
