# Environments — run Komet on a VPS

An **environment** is a workspace backed by its own sync server. Sessions started
in it execute on that machine, and its sidebar is separate from your local one:
no shared registry, room, token, or chat list. Your desktop is only a controller
there, never a device.

Status: shipped — the server, the engine runtime, `komet serve`, and the desktop
side that pastes the link (Settings → Sync → **Add device**) and switches to the
environment from the **Devices** picker at the top of the sidebar. In an
environment's workspace you can also enable and manage that machine's agents
(Settings → Agents targets the device you are looking at).

## One command on the VPS

```bash
komet serve --public-url https://sync.example.com --install-service
```

Or install the machine from scratch, in one line — useful on a VM you have only
an SSH shell on. It installs the CLI, registers the service, and prints the link:

```bash
curl -fsSL https://raw.githubusercontent.com/jomvick/komet/main/install.sh \
  | sh -s -- --serve --lan
```

This generates a token and a namespace once (`~/.komet/serve.json`, mode `0600`),
installs a `systemd --user` (or launchd) service, starts the sync server on
`:8787`, starts the engine that will run this environment's sessions, and prints
a pairing link:

```
komet://pair/eyJ2IjoxLCJsYWJlbCI6...
```

The link is a secret — it grants shell on that host. Paste it in Komet
(Settings → Sync → **Add device**) to add the environment; the desktop walks
through decode, reachability probe, and pairing, and reports which step failed.

Without `--install-service`, the same command runs in the foreground (useful
under Docker or your own supervisor). Without `--public-url` or `--lan`, no link is
printed: expose the server behind TLS first, then run
`komet serve --public-url https://… --print-link`.

### Flags

| Flag | Meaning |
| --- | --- |
| `--public-url URL` | Public HTTPS origin your proxy serves. Baked into the link. |
| `--lan` | No proxy yet: serve plain HTTP on this machine's private address, and say so loudly. |
| `--port N` | Sync server port (default `8787`; saved on first run). |
| `--label NAME` | Name shown in the desktop switcher (default: hostname). |
| `--install-service` | Write + start the user service, then exit. |
| `--print-link` | Print the pairing link and exit (reads the saved config). |

Everything is persisted, so `komet daemon restart` needs no environment variable.
`--public-url` wins over `--lan` and replaces a saved URL. A URL already saved in
`serve.json` wins over `--lan` — re-running with `--lan` never rewrites an address
you chose (a `--lan` address does follow `--port`).

## TLS (required for a link)

The bearer travels in the WebSocket URL (`?token=…`), so never expose plain HTTP
on the internet. Caddy on the same machine:

```
sync.example.com {
    reverse_proxy 127.0.0.1:8787
    log {
        # The token rides in the query string; keep access logs off for `/`.
    }
}
```

Any reverse proxy works: forward WebSocket upgrades for `/session`, `/chat2`,
`/registry`, `/device`, and the blob routes.

`--lan` is the deliberate exception to this rule, for a network you already
control: it prints the warning next to the link, and the desktop pairing dialog
makes you confirm the same thing before it connects.

## What runs where

```
Desktop (controller)                     VPS (environment)
┌──────────────────┐   ws://…/registry  ┌──────────────────────────────┐
│ local engine     │◄──────────────────►│ komet-sync-server (:8787)    │
│  ├ local chats   │                    │ komet serve's engine         │
│  └ env connection│   RPC over relay   │  ├ registry (chats/spaces)   │
└──────────────────┘◄──────────────────►│  └ agent runs / terminals    │
                                        └──────────────────────────────┘
```

- `~/.komet/orgs/{org}/{user}/` — the VPS engine's own docs store.
- `~/.komet/rooms/` — the sync server's rooms (registry rows are SQLite, durable).
- `~/.komet/serve.json` — token, namespace, port, public URL. Secret.
- The desktop keeps one background connection per environment, so the online dot,
  live statuses and notifications keep working while you look at another one.

## Managing

```bash
komet daemon status          # is the service up?
komet daemon restart         # after editing ~/.komet/serve.json
komet daemon uninstall       # stop and remove the unit
```

Data lives in `KOMET_DATA_DIR` (default `~/.komet`). Removing the desktop-side
environment forgets it by default and can delete its local mirror on request.

To retire the machine entirely — the service, its bearer, and every document it
served (this is the only copy of this environment's sessions, so remove the
desktop side first if you want to keep the mirror):

```bash
systemctl --user disable --now komet.service
rm -rf ~/.komet ~/.local/bin/komet
```

## Troubleshooting

```bash
TOKEN=$(python3 -c "import json;print(json.load(open('$HOME/.komet/serve.json'))['token'])")
curl -s -H "authorization: Bearer $TOKEN" http://127.0.0.1:8787/env
journalctl --user -u komet.service -f
```

- `unauthorized` from `/env`: the token in the link does not match `serve.json`.
  Re-print the link with `--print-link` instead of editing the file by hand.
- The environment shows no chats: the desktop must join `/registry/{orgId}/ws`.
  Check the server logs for a `registry room joined` line.
- Port already in use: another komet process owns `8787` — `komet daemon stop`,
  or pass `--port`.
