# Browser-use Komet — Design

Date : 2026-10-12. Statut : proposé. Périmètre : automation browser côté agent + affichage UI, calqué sur freebuff `browser-use`, adapté à la stack Rust/gpui de Komet.

## Contexte vérifié

Freebuff a deux systèmes, un seul vivant :
- Legacy `browser_logs` (mort) : `common/src/tools/params/tool/browser-logs.ts:8-23`, réponse `common/src/browser-actions.ts:52-65`, handler pass-through `packages/agent-runtime/src/tools/handlers/tool/browser-logs.ts:8-21`. Le SDK jette `Tool not implemented` (`sdk/src/run.ts:1409-1411`). À ne pas copier.
- Actuel `browser-use` : `agents/browser-use/browser-use.ts:5-259`. `id:browser-use`, modèle `GEMINI_3_5_FLASH_LITE_MODEL_ID`, `includeMessageHistory:false:127`, `mcpServers:{chrome-devtools:{command:npx,args:[-y,chrome-devtools-mcp@latest,--headless,--isolated]}}:129-134`, `toolNames:[set_output,run_terminal_command,add_message]:136` + outils MCP injectés (`navigate_page,take_snapshot,take_screenshot,click{uid},fill{uid,value},hover,press_key{uid,key},list_console_messages,list_network_requests,evaluate_script,performance_*`). Workflow `Navigate→Snapshot→Plan→Act→Verify→Report`, `take_snapshot` d'abord pour les `uid`, `evaluate_script` en arrow-function obligatoire `:209-224`, sortie via `set_output:245` en `outputSchema:48-125` `{overallStatus:success|failure|partial,summary,finalUrl,results[{name,passed}],consoleErrors[],lessons[]}`. Parent `agents/base2/base2.ts:247` l'a en `spawnableAgents` inconditionnel, spawné via `spawn_agents{agent_type:browser-use,prompt,params:{url}}`. Script manuel `agents/browser-use/run-browser-use.ts` (3 tâches, traces `debug/browser-agent-traces/`).
- UI freebuff : aucun browser intégré. Screenshots MCP restent en JSON texte du sous-agent. Affichage image seulement pour uploads utilisateur (`cli/src/utils/terminal-images.ts`, `cli/src/components/blocks/image-block.tsx`). Gate Chrome par prompt uniquement (`common/src/util/system-info.ts` → `Chrome: installed|not found`), pas de vérif runtime avant `npx`.

Komet (vérifié) : Rust workspace, UI `gpui` natif desktop (`crates/ui/`). Agents via `trait Harness` (`crates/harness/src/lib.rs`), `enum ToolCall` (`crates/proto/src/agent.rs:1147` : `Exec,ReadFile,WriteFile,EditFile,ApplyPatch,Search,Glob,WebFetch,WebSearch,Todo,Mcp{server,tool,input},Unknown`). MCP double existant (`crates/engine/src/mcp/` : catalogue interne + `registry.rs` → `mcp-servers.json`, endpoint per-run `127.0.0.1:0/mcp/agents?callerAgentId`, Bearer sha256). Injection harness via `capabilities.rs:supports_dynamic_mcp` (true pour ClaudeCode,Opencode,Grok,Hermes,Pi,Cline,Mock). UI transcript virtualisé (`crates/ui/src/transcript.rs`, chips `proto/src/view.rs:360`, `ToolDetail{Output,Diff,Stats}` + `GetToolDetail` RPC) + images utilisateur avec lightbox/cache 64MB (`crates/ui/src/attachments.rs`). Aucun browser automatisé (zéro hit `playwright|puppeteer|chrome-devtools|browser-use` hors site vitrine).

## Intent et pourquoi

Donner aux agents Komet la capacité freebuff : vérifier un rendu, tester une webapp locale, extraire une page, remonter `consoleErrors`, sans quitter Komet. Succès = un agent spawne un browser headless isolé, agit (navigate/snapshot/click/fill), et le transcript montre étapes + vignette screenshot cliquable + erreurs console.

## Goals / non-goals / anti-goals

Goals :
1. Phase 1 zéro-code : serveur MCP `chrome-devtools-mcp` déclarable via `SaveMcpServer` (Stdio `npx ... --headless --isolated`), `always_load` ou assigné par chat, injecté aux harnesses compatibles existants.
2. Gate Chrome : probe `installed()` FS (binaires par OS) + message clair si absent, avant le spawn `npx`.
3. UI : étapes browser nichées sous le bloc agent (comme freebuff), `consoleErrors` lisibles, screenshot en vignette → lightbox existante.
4. Sécurité/privacy existantes conservées (Bearer per-run, secrets jamais sur wire, render-parts).

Non-goals : pas de browser visible piloté à la souris en phase 1 ; pas de persistance de profil utilisateur (isolé par défaut, comme freebuff) ; pas de `browser_logs` legacy.
Anti-goals : réussir techniquement mais fuiter un token MCP ou un cookie de session dans le transcript/doc sync ; réussir mais exiger iTerm2/Kitty pour voir quoi que ce soit (Komet est gpui natif, doit afficher en natif).

## Contraintes

- `SandboxOptions deny_unknown_fields` : toute typo de champ = erreur wire.
- Secrets MCP via `McpSecretStore`, `PublicMcpServerConfig` seul sur RPC, `Debug` masqué.
- Render-parts privacy : inputs complets host-locaux, contenus `WriteFile` strippés.
- RPC in-memory = même protocole que WS, pas de shortcut.
- `installed()` = probe FS sans spawn ; repo `~/.komet/worktrees`, diff cap 3MiB.
- `descriptor-stability` : descripteur statique doit égaler `describe()` post-resolve (test `codex_lazy_descriptor_matches_resolved_harness`).
- Budget images : cache transcript LRU 64MB, `flush_evicted` obligatoire (tiles atlas), formats via `ImageFormat` jail + `mime_by_ext`.

## Décisions (figées par cette spec)

1. Phase 1 = config, pas de nouveau binaire : `McpTransport::Stdio{command:npx,args:[-y,chrome-devtools-mcp@latest,--headless,--isolated]}` via `SaveMcpServer` RPC → `engine/src/mcp/registry.rs`. `discovery.rs:fake_tools_list` remplacé par vrai list (ou documenté comme limite phase 1 avec `TestMcpServer/ListMcpTools` manuels).
2. Gate Chrome côté engine : probe chemins par OS (reprendre liste freebuff `system-info.ts:10-40`) dans `installed()` du harness/outil ; si absent, erreur lisible `browser-use requires Google Chrome or Chromium` au lieu de `Failed to load tools from MCP server`.
3. Pas de `ToolCall` browser dédié en phase 1 : réutiliser `ToolCall::Mcp{server:chrome-devtools,tool,input}` + `Unknown` fallback. Normalizers existants (`mcp__<server>__<tool>`) inchangés.
4. UI phase 1 : chip `MCP/Unknown` existant + `ToolDetail::Output` texte (étapes, `consoleErrors`). Screenshots MCP (base64 JPEG) transitent par `UploadChunk/UploadCommit` ou nouveau `ToolDetail` image → vignette + `lightbox()` de `attachments.rs`. Cache via `attachment_snapshot/protect_attachments/seed_attachment`.
5. Phase 2 (native, repoussée) : nouveau `HarnessId::Browser` (`proto/src/agent.rs`) + `harness/src/browser/{mod.rs,normalize.rs}` + `register_lazy` dans `engine/src/registry.rs` + branche sandbox `ARCHITECTURE.md §6` + modèles/cache `model-cache.json`. Seulement si la phase 1 montre des limites (perf, mapping events, turn-end déterministe).

## Laissé au builder (choix d'implémentation)

- Détail du probe Chrome (liste chemins Linux/macOS/Windows, variable `CHROME_PATH` ou non).
- Remplacement de `discovery.rs` fake par vrai client rmcp vs simple passthrough `callMCPTool` : choisir le minimal qui fait passer `TestMcpServer/ListMcpTools`.
- Format exact du transit screenshot (réutiliser `ReadAttachmentChunk` vs étendre `enum ToolDetail`) et calcul `detail_height` pour la vignette.
- Texte exact du `spawnerPrompt` équivalent freebuff (use-cases + responsabilités parent + gate Chrome) et où l'injecter (strings templates vs doc).
- Tâches de test manuel équivalentes à `run-browser-use.ts` (wikipedia/HN/httpbin) adaptées en tests Rust ou script harness.
- Ordre des jalons UI : vignette d'abord ou `consoleErrors` d'abord si budget serré.

## Tests attendus

- `SaveMcpServer` chrome-devtools + `TestMcpServer` + `ListMcpTools` listent `navigate_page,take_snapshot,click,fill,...`.
- Run sans Chrome → erreur lisible, pas de crash `npx`.
- Run avec Chrome → `take_snapshot` puis `click/fill{uid}` passent, `list_console_messages` remonte, output structuré `{overallStatus,summary,results,consoleErrors}`.
- Secrets absents du doc sync/transcript/logs ; Bearer per-run invalidé en fin de session ; isolation inter-agents.
- UI : étapes nichées visibles, vignette ouvre lightbox, budget cache respecté.

## Addendum builder-check (2026-10-12, mes choix)

Vérifié dans le code :
- Spawn : Komet n'a pas de `spawn_agents` engine comme freebuff. Le spawn passe par les outils natifs de chaque harness (`Agent` / `Agent: ...`, `spawn_subagent`, `task{subagent_type,prompt}`), normalisés en `ToolCall::Unknown` avec `is_subagent_spawn()` (`crates/proto/src/agent.rs:1205-1241`, `SUBAGENT_MODEL_KEYS`, trackers `harness/src/acp/subagent.rs`, `subagent_opencode.rs`, viz `AgentEvent::Subagent`). Phase 1 = le parent utilise son mécanisme natif existant ; les outils MCP `chrome-devtools` sont injectés dans la session (parente ou enfant) via `supports_dynamic_mcp`. Aucun nouveau primitif de spawn à construire.
- Sortie structurée : pas d'équivalent `set_output`/`outputSchema` côté engine. Convention phase 1 (mon choix) : l'agent browser termine par un bloc JSON `{overallStatus,summary,finalUrl,results,consoleErrors,lessons}` dans son output final (`AgentEvent::Done`/résultat du subagent), parsé en best-effort côté UI ; pas de validation zod engine en phase 1.
- Harnesses phase 1 (mon choix) : uniquement ceux avec `supports_dynamic_mcp=true` (ClaudeCode, Opencode, Grok, Hermes, Pi, Cline, Mock) ; modèle = celui du chat courant (pas de modèle imposé comme le `GEMINI_FLASH_LITE` de freebuff).
- Chrome/MCP lifecycle (mon choix) : `chrome-devtools-mcp` auto-lance son Chrome avec profil isolé per-run ; pinning `chrome-devtools-mcp@<version>` au lieu de `@latest` (repro + supply-chain), version fixée au moment de l'implémentation.
- Screenshots (mon choix par défaut) : traités comme pixels de page non fiables, locaux uniquement (`protect_attachments`, jamais en doc sync), en attente de décision produit.

Reste au produit (reporté dans l'issue) : prérequis Node/npx + règle sandbox pour Chrome ; pré-seedé/`always_load` vs ajout manuel ; politique privacy exacte des screenshots.
