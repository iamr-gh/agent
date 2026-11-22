# One running agent, several interfaces

**Recommendation for `ag`:** Build a responsive browser control panel backed by a persistent, single-owner supervisor on the cloud host. Follow it with a terminal client to the same supervisor. Keep Discord as an optional notification and deep-link surface, not the thing that owns a run. Use SSH + tmux as a fallback for direct terminal sessions, not as the protocol for the new interfaces.

## What the existing products show

This is a review of published documentation, not a hands-on product or accessibility audit.

| Approach | What it gets right | What it means for `ag` |
| --- | --- | --- |
| [DreamCoder announcement](https://www.masteringemacs.org/article/announcing-dreamcoder) and [product site](https://www.dreamcoder.ai) | A self-hosted browser IDE, not a tmux wrapper. Search-first commands, keyboard and mouse navigation, tiled layouts, and a workspace minimap that shows what is busy or blocked. The product site describes a reflowed UI reaching the same instance on phone and laptop. | Borrow the attention cues and keyboard navigation first, not the tiling engine. DreamCoder has its own tool loop; its config importer is not live `ag` session compatibility. |
| [OpenCode web](https://opencode.ai/docs/web/) and [server API](https://opencode.ai/docs/server/) | A concrete server-first design: clients get session data and events and can send explicit prompt, interrupt, and permission actions instead of simulating keystrokes. | Closest architectural reference. A reasonable alternative to try if replacing `ag` is acceptable; not a drop-in frontend for its sessions. |
| [Claude Code Remote Control](https://code.claude.com/docs/en/remote-control) and [Happy](https://github.com/slopus/happy) | Claude exposes a running host session via vendor-relayed web and mobile clients; the host and process still have to stay up. Happy documents mobile/web clients, encrypted session sync, and support for other harnesses via ACP. | Good references for phone handoff and notifications. Neither has verified `ag` compatibility. If considering Happy, test live handoff and approval behavior with the exact harness, not just whether it can launch it. |
| [SSH + tmux](https://github.com/tmux/tmux/wiki/Getting-Started), optionally [Mosh](https://mosh.org/) | Reattach to the actual terminal process rather than restoring a transcript; Mosh helps on flaky or roaming connections. | Useful today on a trusted laptop. It mirrors a terminal, not structured attention, approval, or diff views; touch and assistive-technology ergonomics depend on the terminal client. |
| [Agent Client Protocol](https://agentclientprotocol.com/protocol/session-setup) | A reusable agent/editor protocol with session lifecycle and update semantics. | Consider an adapter once the core works. ACP session loading/resuming is not by itself multi-device live fan-out, remote identity, or reliable reconnect. |

## What each device needs

| Device | Best default |
| --- | --- |
| Personal phone | A responsive browser page bookmarked to **Needs attention**. Read what is blocked, review the complete action, approve/deny, stop, or send a short follow-up. Keep lock-screen notifications generic and link back to the session. No terminal or slash commands required. |
| Trusted laptop | The same browser session with a searchable switcher, keyboard shortcuts, transcript, diff and tool details. Add an SSH-friendly attach client for working in a terminal without starting a second `ag` process. Add split panes only if the simpler layout becomes limiting. |
| Borrowed but trusted computer | An install-free browser behind HTTPS and an identity-aware gateway, separate from the private-network default. Use short-lived, revocable, read-only access to a deliberately limited status view; no transcripts, secrets, approvals, or shell. No agent access from a kiosk or otherwise untrusted device; use the phone instead. |
| Discord or other messaging | Opt-in notices for blocked/done/failed and links to the canonical session. If keeping interactive Discord controls, move them to the shared supervisor and expose buttons rather than requiring slash commands. |

The browser should work with keyboard, touch, and screen readers: state as text rather than color alone, visible focus, labelled controls, no keyboard trap, and announcements for milestones rather than every streamed token. Test on a real phone and with a screen reader before calling it accessible.

## Fit with the current code

[Saved sessions](SESSIONS.md) already preserve model context and protect it with one-writer locks; a second process cannot use `--resume` to attach to a live one. [The Discord integration](DISCORD.md) already has a frontend-neutral `ag --worker` JSON-lines protocol and a reusable subprocess runner. Today Discord owns its active work, queue, and pending approvals; a Discord disconnect cancels those turns. The worker's complete-answer and tool events are enough for a first status/approval UI without adding token streaming. Its live events are not a durable replay log.

1. **First slice: web supervisor for new managed sessions.** Keep it on one host, reuse worker subprocesses and existing session/workspace locks, and start with manual approvals. Give each managed session a stable ID and server-owned status, active turn and pending approval. Use ordinary HTTP for actions and server-sent events for updates. A reconnect gets a fresh snapshot and resumes from a sequenced event cursor where available; a missed cursor triggers a snapshot refresh, not another prompt. This need not take over an already-running standalone terminal session.
2. **Prove the handoff before adding clients.** Start on laptop, close the tab mid-turn, open the same URL on phone, observe the same pending approval and grant it once, then return to laptop and see the result. Also check that a stale or duplicate approval is rejected, an expired approval fails closed, an unauthorized client cannot act, and stopping does not imply rollback. Keep the run alive on *client* disconnect; preserve current recovery semantics on *worker/host* failure and never auto-replay effects. Until queues are explicitly designed, reject new prompts while busy instead of silently dropping or duplicating them.
3. **Add more ways in, not more owners.** Route terminal attach and Discord through the same API. For personal devices, start with loopback plus an SSH tunnel or private HTTPS via [Tailscale Serve](https://tailscale.com/kb/1312/serve); Serve alone is not the browser-only borrowed-device path. Keep the raw worker and shell off the network, enforce permissions on the server, and use a restricted execution account before expanding remote access. The current workspace lease is not a sandbox.

The first useful screen is an inbox of **needs approval / running / done / failed** across sessions, not another chat sidebar. A useful early success metric: can you tell what needs you and take the right action from your phone without typing a command or hunting through threads?
