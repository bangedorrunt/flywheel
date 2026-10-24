-- One-shot session snapshot for wake acceleration.
--
-- Hint only, never the source of truth: the inbox log decides. This script
-- must stay handler- and timer-free (`rex.on`, `rex.every`, `rex.after`,
-- `rex.wait` all keep `rex do` running). It runs once and exits with the
-- JSON below, which the caller parses with the same shape as `session.view`.
local view, err = rex.session.view{}
if err ~= nil then error(err, 0) end
return { label = view.label, windows = view.windows }
