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
    icon: `<path d="M15.914 4a1.5 1.5 0 00-2.474-1.561l-9 9A1.5 1.5 0 005.5 14h4.002a.5.5 0 01.471.666L8.086 20a1.5 1.5 0 002.475 1.56l9-9A1.5 1.5 0 0018.5 10h-3.997a.5.5 0 01-.472-.667z"/>`,
  },
  {
    tag: "Multi-agent",
    title: "Chaque agent, une seule timeline",
    body: "Claude Code, Codex, Cursor, Grok, Hermes, OpenCode, Pi — chacun branché sur son interface native la plus solide, normalisé dans un seul modèle de session.",
    icon: `<path d="M12.83 2.18a2 2 0 0 0-1.66 0L2.6 6.08a1 1 0 0 0 0 1.83l8.58 3.91a2 2 0 0 0 1.66 0l8.58-3.9a1 1 0 0 0 0-1.83z"/><path d="M2 12a1 1 0 0 0 .58.91l8.6 3.91a2 2 0 0 0 1.65 0l8.58-3.9A1 1 0 0 0 22 12"/><path d="M2 17a1 1 0 0 0 .58.91l8.6 3.91a2 2 0 0 0 1.65 0l8.58-3.9A1 1 0 0 0 22 17"/>`,
  },
  {
    tag: "Historique",
    title: "Rewind qui tient sa promesse",
    body: "Chaque prompt pose un checkpoint sur ton arbre de travail via une référence git cachée. Reviens en arrière sur le code et sur la conversation, pas seulement sur le chat.",
    icon: `<path d="M3 12a9 9 0 1 0 9-9 9.75 9.75 0 0 0-6.74 2.74L3 8"/><path d="M3 3v5h5"/><path d="M12 7v5l4 2"/>`,
  },
  {
    tag: "Clavier",
    title: "Pensé pour ne jamais lâcher le clavier",
    body: "⌘N ouvre une session, ⏎ met en file la suite pendant que l'agent travaille, ⌘⏎ pilote en plein tour, Échap arrête. Aucune action n'exige la souris.",
    icon: `<path d="M15 6v12a3 3 0 1 0 3-3H6a3 3 0 1 0 3 3V6a3 3 0 1 0-3 3h12a3 3 0 1 0-3-3"/>`,
  },
  {
    tag: "Local-first",
    title: "Local par architecture, pas par option",
    body: "Projets, sessions, transcripts et identifiants restent sur le disque. Aucun compte requis, aucune télémétrie, aucun cloud entre toi et tes agents.",
    icon: `<path d="M10 16h.01"/><path d="M2.212 11.577a2 2 0 0 0-.212.896V18a2 2 0 0 0 2 2h16a2 2 0 0 0 2-2v-5.527a2 2 0 0 0-.212-.896L18.55 5.11A2 2 0 0 0 16.76 4H7.24a2 2 0 0 0-1.79 1.11z"/><path d="M21.946 12.013H2.054"/><path d="M6 16h.01"/>`,
  },
  {
    tag: "Sync (roadmap)",
    title: "Multi-device quand tu le décides",
    body: "Les briques existent déjà — CRDT Loro, relais par appareil, pièces synchronisées — mais restent désactivées par défaut. Une VPS peut garder tes agents actifs pendant que ton laptop se déconnecte.",
    icon: `<path d="M3 12a9 9 0 0 1 9-9 9.75 9.75 0 0 1 6.74 2.74L21 8"/><path d="M21 3v5h-5"/><path d="M21 12a9 9 0 0 1-9 9 9.75 9.75 0 0 1-6.74-2.74L3 16"/><path d="M8 16H3v5"/>`,
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
    detail: "Script d'installation automatique, AppImage autonome ou tarball.",
    command: "curl -fsSL https://raw.githubusercontent.com/jomvick/komet/main/install.sh | sh",
  },
  {
    os: "macOS",
    detail: "Image disque .dmg (Apple Silicon) ou service launchd via CLI.",
    command: "komet daemon install",
  },
  {
    os: "Windows",
    detail: "Exécutable autonome (.exe) ou archive .zip prête à l'emploi.",
    command: "komet.exe",
  },
];
