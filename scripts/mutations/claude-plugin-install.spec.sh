# Sourced by scripts/verify-mutations.sh. bd-9os20: the plugin is delivered by
# stack.sh into the path spawn passes with --plugin-dir.

CARGO_ARGS=(-p flywheel --test proof_steps)
TEST_FILTER=(proof_claude_plugin_loads_and_installs_to_plugin_dir)

mutation "P1  stack.sh stops installing the Claude plugin" \
    file=scripts/stack.sh \
    expect=proof_claude_plugin_loads_and_installs_to_plugin_dir \
    find='    run cp -R "$plugin_src" "$plugin_dir"' \
    replace='    run : # bd-9os20 mutant: no plugin copy'

mutation "P2  the plugin manifest stops declaring its module" \
    file=plugins/claude-flywheel/hooks/hooks.json \
    expect=proof_claude_plugin_loads_and_installs_to_plugin_dir \
    find='{ "modules": ["./register.ts"] }' \
    replace='{ "modules": [] }'

mutation "P3  spawn points --plugin-dir at the wrong path" \
    file=src/cmd/spawn.rs \
    expect=proof_claude_plugin_loads_and_installs_to_plugin_dir \
    find='        format!("{data_home}/flywheel/plugins/claude-flywheel"),' \
    replace='        format!("{data_home}/flywheel/plugins/other"),'
