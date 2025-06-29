#!/usr/bin/env bash

set -e -x
CUE=~/cue_v0.14.0-alpha.1_darwin_arm64/cue

# This file allows to incremental manual testing of our cue helper files

cd "$(dirname "$0")" || exit 3

pushd lib
$CUE eval helpers.cue parameters.cue tests/parametersTest.cue --expression  '[Test.Params]'
$CUE eval -t eval helpers.cue parameters.cue parameterToContainerBindings.cue manifestHelpers/eval.cue template.cue tests/templateTest.cue --expression  '[Test.Templates]'