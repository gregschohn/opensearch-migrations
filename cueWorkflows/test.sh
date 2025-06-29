#!/usr/bin/env bash

set -e -x

# This file allows to incremental manual testing of our cue helper files

cd "$(dirname "$0")" || exit 3

pushd lib
cue eval helpers.cue parameters.cue tests/parametersTest.cue --expression  '[Test.Params]'
cue eval -t eval helpers.cue parameters.cue parameterToContainerBindings.cue manifestHelpers/eval.cue template.cue tests/templateTest.cue --expression  '[Test.Templates]'