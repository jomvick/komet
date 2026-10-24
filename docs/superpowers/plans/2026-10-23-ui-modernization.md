# Modernisation UI Komet — plan d'implémentation

## Direction

Komet reste un outil de travail dense et calme. Les icônes en contour portent
la navigation et la structure ; les surfaces pleines marquent une sélection,
une action ou un état qui exige l'attention. Les animations expliquent un
changement d'état et ne constituent pas une préférence configurable.

## Première itération

### Parcours et critères

| Critère | Résultat observable | Vérification |
|---|---|---|
| AC-001 | Les réglages sont regroupés dans les huit catégories demandées ; catégories et entrées ont des icônes distinctes. | Test unitaire des groupes et inspection de l'interface. |
| AC-002 | La section active est plus visible sans confondre la navigation inactive : fond plein, icône contrastée et libellé accentué. | Inspection manuelle à 900px et 1440px. |
| AC-003 | Le raisonnement en direct ne pousse plus le transcript : il affiche un aperçu replié puis se développe au clic. | Test unitaire de l'état et inspection durant un flux mock. |
| AC-004 | Les contrôles restent lisibles et utilisables au clavier, et les animations respectent la préférence système de réduction de mouvement. | Inspection clavier et chemin reduced-motion existant. |
| AC-005 | Les chemins et références `fichier:ligne` ou `fichier#Lx-Ly` ouvrent le fichier dans Komet et positionnent l'éditeur à la ligne. | Tests du parseur et du calcul d'offset, inspection dans un workspace mock. |

### Portée et validation restante

- La navigation par référence est reliée au contexte workspace et aux onglets
  de fichiers ; les chemins absolus et traversées de répertoire restent du
  texte ordinaire.
- Les mesures comparatives d'ouverture, streaming, transcript long et panneaux,
  ainsi que l'inspection aux largeurs/échelles Linux, restent à exécuter dans
  une session graphique instrumentée. Cette tranche conserve la virtualisation,
  le cache de rendu et les animations reduced-motion existants ; elle ne prétend
  pas fournir de mesures de performance non recueillies.

## TDD

1. Ajouter les tests de structure des catégories Settings et de comportement
   du raisonnement replié ; constater l'échec de compilation attendu.
2. Ajouter les modèles et le rendu minimal pour satisfaire ces tests.
3. Exécuter les tests ciblés `komet-ui`, puis formatter et vérifier la crate.

## États couverts

| État | Comportement |
|---|---|
| Raisonnement vide | Aucun panneau rendu. |
| Raisonnement actif replié | Une ligne de statut et un aperçu borné restent visibles. |
| Raisonnement développé | Le contenu entier reste dans le transcript, sans modifier les messages persistés. |
| Réduction de mouvement | Les animations gpui existantes se figent selon la préférence système. |
