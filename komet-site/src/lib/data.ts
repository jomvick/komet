export const agents = [
  "Claude Code",
  "Codex",
  "Cursor",
  "Grok",
  "Hermes",
  "OpenCode",
  "Pi",
];

export const features = [
  {
    tag: "Moteur",
    title: "Rust + gpui, un seul binaire",
    body: "Le même moteur qui fait tourner Zed. Lancement instantané, défilement fluide même sur des années de transcript, aucune fenêtre Electron à réchauffer.",
  },
  {
    tag: "Multi-agent",
    title: "Chaque agent, une seule timeline",
    body: "Claude Code, Codex, Cursor, Grok, Hermes, OpenCode, Pi — chacun branché sur son interface native la plus solide, normalisé dans un seul modèle de session.",
  },
  {
    tag: "Historique",
    title: "Rewind qui tient sa promesse",
    body: "Chaque prompt pose un checkpoint sur ton arbre de travail via une référence git cachée. Reviens en arrière sur le code et sur la conversation, pas seulement sur le chat.",
  },
  {
    tag: "Clavier",
    title: "Pensé pour ne jamais lâcher le clavier",
    body: "⌘N ouvre une session, ⏎ met en file la suite pendant que l'agent travaille, ⌘⏎ pilote en plein tour, Échap arrête. Aucune action n'exige la souris.",
  },
  {
    tag: "Local-first",
    title: "Local par architecture, pas par option",
    body: "Projets, sessions, transcripts et identifiants restent sur le disque. Aucun compte requis, aucune télémétrie, aucun cloud entre toi et tes agents.",
  },
  {
    tag: "Sync (roadmap)",
    title: "Multi-device quand tu le décides",
    body: "Les briques existent déjà — CRDT Loro, relais par appareil, pièces synchronisées — mais restent désactivées par défaut. Une VPS peut garder tes agents actifs pendant que ton laptop se déconnecte.",
  },
];

export const stats = [
  { label: "Démarrage", value: "< 100ms", note: "binaire natif, pas de VM à chauffer" },
  { label: "Défilement", value: "60fps", note: "des années de transcript, sans à-coup" },
  { label: "Réseau par défaut", value: "0 appel", note: "mode local strict, hors sync activée" },
  { label: "Binaire", value: "1 seul", note: "moteur + interface, headed ou headless" },
];

export const faq = [
  {
    q: "Komet est-il un client Electron de plus ?",
    a: "Non. L'interface tourne sur gpui — le framework GPU-accéléré derrière Zed — et le moteur est un daemon Rust pur qui fonctionne aussi bien en tête (fenêtre) qu'en headless sur un serveur.",
  },
  {
    q: "Faut-il de nouvelles clés API ?",
    a: "Non. Komet pilote les CLI d'agents que tu as déjà installées et connectées — il ne remplace pas tes accès, il les orchestre.",
  },
  {
    q: "Où vivent mes données ?",
    a: "Sur ta machine. Chaque appareil fait tourner un petit moteur qui stocke ses sessions localement. Une nouvelle installation démarre en mode local uniquement, sans compte ni connexion réseau.",
  },
  {
    q: "Et la synchronisation multi-appareils ?",
    a: "Elle fait partie de la feuille de route. Le code existe déjà (CRDT Loro via des relais par appareil) mais reste désactivé par défaut — tu l'actives explicitement quand tu es prêt.",
  },
];

export const platforms = [
  {
    os: "Linux",
    detail: "Script d'installation, daemon persistant au démarrage.",
    command: "curl -fsSL https://komet.sh/install.sh | sh",
  },
  {
    os: "macOS",
    detail: "Build depuis les sources, ou service launchd via la CLI.",
    command: "komet daemon install",
  },
  {
    os: "Windows",
    detail: "Installeur .msi signé, ou exécutable portable autonome.",
    command: "komet.exe --service",
  },
];
