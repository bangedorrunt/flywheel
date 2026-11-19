# Sourced by scripts/verify-mutations.sh.
# Keybinds in chat history must not open an idle seat's palette.

CARGO_ARGS=(--bin flywheel)
TEST_FILTER=(every_captured_screen_classifies)

mutation "A1  the palette scan is not anchored above the bottom box" \
    file=src/pane_screen.rs \
    expect=every_captured_screen_classifies \
    find='    let lines: Vec<&str> = tail.lines().collect();
    let is_box = |line: &str| line.trim_start().starts_with('"'"'┃'"'"');
    let mut i = lines.len();
    while i > 0 && !is_box(lines[i - 1]) {
        i -= 1;
    }
    while i > 0 && is_box(lines[i - 1]) {
        i -= 1;
    }
    let mut above: Vec<&str> = Vec::new();
    while i > 0 && !is_box(lines[i - 1]) {
        i -= 1;
        above.push(lines[i]);
    }
    let header = above
        .iter()
        .any(|line| trim_box_edge(line.trim_start()) == "Search")
        && above
            .iter()
            .any(|line| matches!(trim_box_edge(line.trim_start()), "Suggested" | "Commands"));
    let keybinds = above
        .iter()
        .filter(|line| {
            line.find("ctrl+x").is_some_and(|at| {
                line[..at]
                    .chars()
                    .rev()
                    .take_while(|c| c.is_whitespace())
                    .count()
                    >= 4
            })
        })
        .count();
    header && keybinds >= 2' \
    replace='    tail.lines().filter(|line| line.contains("ctrl+x")).count() >= 2'
