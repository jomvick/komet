# komet-site

Site vitrine du projet **komet** (contrôleur natif d'agents de code), construit avec :

- **Next.js 16** (App Router, TypeScript)
- **Tailwind CSS v4**
- **Framer Motion** pour les animations (hero, scroll-reveal, FAQ)
- Police **Geist / Geist Mono** (identique à l'app native komet, auto-hébergée via le package `geist`)

## Direction artistique

La palette, les rayons de bordure (10px panneaux / 6px contrôles / 16px bulles) et les tons de
texte sont repris tels quels de `crates/ui/src/theme.rs` (thème sombre de l'app komet) pour que
le site se sente comme une extension directe de l'application :

- fond `#060606`, surfaces `#0d0d0d` → `#262626`
- accent indigo `#7c86ff` (indigo-400, identique à l'accent de l'app)
- rose "busy" `#f472b6` pour l'indicateur "working" (clin d'œil à `thinking_orbs.rs`)
- violet `#c4b5fd` pour le code inline

L'élément signature de la page est la **carte de session animée** dans le hero : un transcript
qui s'écrit, des "thinking orbs" qui pulsent, et un scrubber de rewind avec des checkpoints git —
ce sont les trois fonctionnalités réelles les plus caractéristiques de komet (multi-agent,
feedback en temps réel, rewind git), pas une décoration générique.

Structure de page inspirée de [waku.sh](https://waku.sh) (hero → pourquoi natif → FAQ) enrichie
avec des patterns de [kopuz.moe](https://kopuz.moe) (bandeau d'intégrations, section "bâti pour
la vitesse", cartes d'installation par OS avec commande à copier).

## Développement

```bash
npm install
npm run dev
```

## Build de production

```bash
npm run build
npm start
```
