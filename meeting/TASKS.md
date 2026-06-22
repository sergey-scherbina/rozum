# meeting — .ssc client task tracker

(Tracked here in the worktree because the shared master `SPRINT.md` checkout is
churned by sibling agents switching branches — commits there don't stick.)

## Done
- Pure .ssc → Rust meeting PWA; hand-written removed.
- Rich text: code/bold/links/badges/date-dividers/timestamps/per-handle colour.
- Dynamic rooms (http prefix routing) + `<select>` switcher.
- `/manage`: rooms list/switch/create/delete (project rooms protected), models list/rm, agents view.

## Management round 2 (2026-06-22, operator: "все задачи в спринт и делай")
- [ ] Bulk cleanup of junk rooms — one button: delete all EMPTY global rooms (0 msgs); project rooms safe.
- [ ] Gateway active model — show current + switch (restart gateway with new --model).
- [ ] Stop / launch agents — launch via `rozum launch`, stop via kill; feasibility TBD (processes).
