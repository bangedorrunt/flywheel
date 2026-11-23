#!/usr/bin/env bash
# bd-mxwzn: workflow-list tests must read an injected catalog root, never the
# operator's ~/.config/flywheel. R1 swaps the root for the ambient config dir
# and the behavioral witness must go RED.
set -uo pipefail

CARGO_ARGS=(--bin flywheel)
TEST_FILTER=(catalog_merge_respects_the_injected_config_root)

mutation "R1  injected catalog root is ignored" \
  file=src/workflow_catalog.rs \
  expect=catalog_merge_respects_the_injected_config_root \
  find='        None => WorkflowCatalog::load_from(config_root.to_path_buf())?,' \
  replace='        None => WorkflowCatalog::load_from(crate::config::config_dir())?,'
