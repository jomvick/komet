# MCP interne Komet (MVP1) — Design

Date : 2026-09-05. Statut : approuvé (oral). Périmètre : serveur MCP interne seul, sans serveurs externes.

## Contexte vérifié

Komet ne gère que l'affichage MCP : `ToolCall::Mcp { server, tool, input }` (`crates/proto/src/agent.rs:1098`), normalisation `mcp__<server>__<tool>` dans chaque harness (`claude/normalize.rs:130`, `codex/normalize.rs:190`, `cursor/mod.rs:729`, `antigravity/normalize.rs:150`), chip `"server · tool"` (`proto/src/view.rs:365`), rendu (`ui/src/transcript.rs:384`). Aucun Registry/Supervisor/Catalog/Policy, aucun client stdio/HTTP/SSE. Modèle repris de Paseo : catalogue partagé indépendant du transport (`paseo-tools.ts:541`), wrapper MCP (`mcp-server.ts:31`), injection runtime HTTP + Bearer + `callerAgentId` avec strip anti double-injection (`runtime-mcp-config.ts:29`), natif-vs-fallback (`architecture.md:390`), flags `daemon.mcp.enabled / injectIntoAgents` (`public-docs/mcp.md`).

## Décisions figées

1. MVP1 = serveur MCP interne Komet seul (pas de stdio/HTTP/SSE externes).
2. Transport = endpoint HTTP local style Paseo (`/mcp/agents?callerAgentId`, Bearer par instance, bind `127.0.0.1`), injection runtime jamais persistée. Le mode natif in-proc est repoussé (optimisation post-MVP1).
3. Permissions = allowlist fermée : lectures `allow`, mutations/terminal/fichiers/permissions `ask` avec mémo session (une fois/session/toujours/refuser), `deny` explicite prioritaire.

## Architecture MVP1

Nouvelle couche `engine::mcp` avec trois unités aux frontières nettes : `McpCatalog` (définition + `executeTool` validé par schéma, handlers internes uniquement), `McpPolicy` (table `(serveur="komet", tool) → allow|ask|deny`, défaut selon §décisions), endpoint HTTP localhost éphémère (token Bearer par instance, `callerAgentId` obligatoire). Statuts `starting/ready/error/stopped` exposés via `watch_mcp_status`. `ToolCall::Mcp` reste affichage/historique pur ; les inputs complets sensibles restent host-locaux (règle render-parts existante) et aucun secret n'entre en doc synchro ni transcript.

## Composants et RPC

- `McpCatalog` : `tools: Map<nom, {title, description, inputSchema, handler}>`, `getTool/executeTool`.
- `McpPolicy` : défaut lectures `allow`, mutations `ask`, `deny` explicite gagnant ; branchement sur le canal de permission existant (ex. `--permission-prompt-tool stdio` côté Claude).
- Injection : `strip` interne puis `withRuntime(komet, agentId, token)` à chaque création de session.
- RPC MVP1 : `list_mcp_servers` (entrée unique `komet` : état, nb tools, dernière erreur), `list_mcp_tools`, `reconnect_mcp_server` (nouveau token, ancien invalidé), `watch_mcp_status`. `add/update/remove/set_enabled` repoussés au MVP2 (externes).

## Data-flow et erreurs

`CreateSession → strip → withRuntime → harness ACP inchangé → appel MCP → vérif Bearer+callerAgentId → Policy → Catalog.executeTool → ToolCall::Mcp` (serveur/tool/durée/résultat/erreur lisible). `starting→ready` nominal ; `error` (bind impossible, auth invalide, timeout handler 30 s, JSON invalide) remonté en transcript + statut ; `stopped` en fin de session avec destruction du token. Pas de backoff auto en MVP1 : `reconnect` explicite.

## Sécurité

Bind strict `127.0.0.1`, token en mémoire seule (masqué en logs), `callerAgentId` isolant les agents, limites taille payload/durée/retour dès MVP1, nouveau token à chaque `reconnect`.

## Tests MVP1

Handshake + découverte ; lecture `allow` sans prompt ; mutation `ask` (approve/deny/mémo session) ; Bearer invalide rejeté ; isolation inter-agents (rejouage token de A par B refusé) ; aucun secret en doc/transcript ; `reconnect` invalide l'ancien token ; affichage transcript (serveur, tool, durée, erreur lisible).

## Jalons suivants

MVP2 : externes `stdio` (Supervisor process, timeouts, crash recovery, backoff, `add/update/remove`). MVP3 : UI Settings + logs. MVP4 : HTTP/SSE distants, resources/prompts. MVP5 : sync contrôlée des configs. Garde-fou : toute tool mutante naît en `ask`.
