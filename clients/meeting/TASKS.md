# meeting — .ssc client task tracker

(Tracked here in the worktree because the shared master `SPRINT.md` checkout is
churned by sibling agents switching branches — commits there don't stick.)

## Done
- Pure .ssc → Rust meeting PWA; hand-written removed.
- Rich text: code/bold/links/badges/date-dividers/timestamps/per-handle colour.
- Dynamic rooms (http prefix routing) + `<select>` switcher.
- Room transcript path hardening: project rooms read `<project>/.rozum/room`;
  ad-hoc rooms honor `$XDG_STATE_HOME`/`$HOME`.
- `/manage`: rooms list/switch/create/delete (project rooms protected), bulk clean-empty,
  models list/rm, gateway status/switch/stop/unload, model-participant start/stop.

## Management round 2 (2026-06-22, operator: "все задачи в спринт и делай")
- [x] Bulk cleanup of junk rooms — one button deletes EMPTY global rooms; project rooms safe.
- [x] Gateway active model — show current model, switch loaded model, stop, unload.
- [x] Model participant controls — start/stop `rozum meetings participant` per room.
- [~] Generic interactive `rozum launch` agents from the web — deferred until there is an explicit
      non-TTY supervisor contract; a TTY program should stay CLI-only for now.
- [ ] Replace shell/Python management actions with daemon REST management endpoints when those APIs exist.
