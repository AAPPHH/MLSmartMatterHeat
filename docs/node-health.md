# Node health (`crates/heatd/src/node_health.rs`, config `[node_health]`)

heatd detects, repairs and reports thermostat failures itself. One state per node, evaluated every `tick_s`:

| state | condition |
|---|---|
| ok | reports arrive and the subscription pushes |
| degraded | subscription lost while polls still answer (attribute pushes alone are not evidence: an unchanged thermostat sends none) |
| silent | no report of any source (push, poll, event) for `silent_min`; a node that never reported counts from heatd start |
| unreachable | silent for `unreachable_min` |

Every transition writes a lake event `node_health` (aspect `link`, from, to, silent_s, episode_s, attempts, last_action, last_error).

## Recovery ladder (state not ok)

- degraded: resubscribe after `[stale] resubscribe_after_s`, then at most every `resubscribe_gap_s`.
- silent / unreachable: resubscribe first `min_gap_s` after entering the state, then with backoff `backoff_start_s` doubling up to `backoff_max_min`. Never faster than `min_gap_s`.
- unreachable: the first attempt of an episode becomes `restart_session` (the interactive chip-tool is restarted, dropping every CASE session; all nodes subscribe again). At most `max_restarts` per unreachable episode and node, at most once per `reset_gap_min` for the whole controller; afterwards only resubscribe, with backoff growing up to `unreachable_backoff_max_min`.
- after a `restart_session` the ladder is suppressed for every node for `restart_grace_s` (chip-tool resubscribes by itself). A new episode starts its gap timer fresh.
- poll reads stay with the capture poll loop (own failure backoff, 60 s base, capped at 16x); its outcome lands in `last_error` as `poll: ...`.

## Setpoint verification

If the device-reported `occupied_heating_setpoint` differs from the value heatd commanded (more than `tolerance_c`, no manual hold, report newer than the write) for `verify_min` while the node answers, heatd writes it again with backoff (`backoff_start_s` doubling). Each rewrite and each failed write is counted (`heatd_node_setpoint_rewrites_total`, `heatd_node_setpoint_failures_total`, event aspect `setpoint`). After `alert_retries` ineffective rewrites or consecutive failed writes the setpoint alert is raised; it clears when the device matches or a write succeeds.

## Reporting

- `/health`: `nodes[].health` (state, since_ts, since_age_s, attempts, attempts_total, last_attempt_age_s, last_action, last_error, setpoint{...}); `status` is degraded while any node is not ok.
- `/api/state`: `rooms[].nodes[].health` and top-level `alerts`.
- `/metrics`: `heatd_node_health`, `heatd_node_health_since_seconds`, `heatd_node_recovery_attempts_total`, `heatd_node_health_transitions_total`, `heatd_node_setpoint_mismatch`, `heatd_node_setpoint_rewrites_total`, `heatd_node_setpoint_failures_total`, `heatd_node_alert`.
- `/api/scan` and `/api/scans`: `alerts` (only present while an alert is active; list of `{id, title, text}`: router watchdog first, then `node_unreachable:<node>@<since_ms>` and `setpoint_not_applied:<node>@<since_ms>`). `alert` stays and carries the first entry, so old clients show it.
- Daily report: section "Thermostat-Gesundheit".
