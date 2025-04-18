// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package tefmocheck

import (
	"encoding/json"
	"fmt"
	"os"

	"go.fuchsia.dev/fuchsia/tools/build"
	"go.fuchsia.dev/fuchsia/tools/testing/runtests"
)

func LoadSwarmingTaskSummary(path string) (*SwarmingTaskSummary, error) {
	data, err := os.ReadFile(path)
	if err != nil {
		return nil, fmt.Errorf("failed to read swarming task summary file %q", path)
	}

	var ret SwarmingTaskSummary
	if err := json.Unmarshal(data, &ret); err != nil {
		return nil, fmt.Errorf("failed to unmarshal swarming task summary: %w", err)
	}
	if ret.Results == nil {
		return nil, fmt.Errorf("swarming task summary did not contain top level `results`. Loaded from path: %s", path)
	}
	return &ret, nil
}

func LoadTestSummary(path string) (*runtests.TestSummary, error) {
	if path == "" {
		return &runtests.TestSummary{}, nil
	}
	data, err := os.ReadFile(path)
	if err != nil {
		return nil, fmt.Errorf("failed to read test summary file %q", path)
	}

	var ret runtests.TestSummary
	if err := json.Unmarshal(data, &ret); err != nil {
		return nil, fmt.Errorf("failed to unmarshal test summary: %w", err)
	}
	return &ret, nil
}

func IgnoreTestsForFlakeAnalysis(tests []runtests.TestDetails, checkTests []runtests.TestDetails) []runtests.TestDetails {
	if len(checkTests) == 0 || !hasFailingTest(checkTests) {
		return tests
	}
	for i, test := range tests {
		if runtests.IsFailure(test.Result) {
			tests[i].Tags = append(tests[i].Tags, build.TestTag{Key: "flake_analysis_ignore", Value: "true"})
		}
	}
	return tests
}

func hasFailingTest(tests []runtests.TestDetails) bool {
	for _, test := range tests {
		if runtests.IsFailure(test.Result) {
			return true
		}
	}
	return false
}
