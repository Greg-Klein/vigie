# vigie

```
        _       _
 __   _(_) __ _(_) ___
 \ \ / / |/ _` | |/ _ \
  \ V /| | (_| | |  __/
   \_/ |_|\__, |_|\___|
          |___/
```

`vigie` surveille GitLab et écrit dans un fichier les tickets qui te sont assignés avec des labels et un statut donnés. Elle n'appelle aucun modèle et n'écrit rien sur GitLab : elle ne fait que lire, à travers `glab`.

Le fichier est fait pour être relu par un autre outil : un script, un tableau de bord, un agent qui prend les tickets à faire.

## Prérequis

- [`glab`](https://gitlab.com/gitlab-org/cli), connecté au compte qui voit les projets (`glab auth login`).
- Le champ statut des tickets, disponible sur les offres payantes de GitLab.
- Rust, pour compiler.

## Installation

```bash
cargo install --path .
```

## Utilisation

```bash
vigie setup                          # configuration pas à pas
vigie add <groupe>/<projet>          # surveiller un projet
vigie add <groupe> --group           # ou tous les projets d'un groupe
vigie set label "equipe-a, frontend" # labels exigés
vigie set status "To do"             # statut exigé
vigie set interval 60                # fréquence d'interrogation, en secondes
vigie set output ~/tickets.json      # fichier à écrire
vigie check --print                  # un passage, sans rien écrire
vigie start                          # surveillance en arrière-plan
vigie status
vigie stop
```

`--label`, `--status` et `--assignee` sur `vigie add` remplacent le filtre commun pour ce projet.

## Le filtre

Un ticket est retenu quand il est ouvert, assigné à toi (ou au compte donné par `assignee`), qu'il porte **tous** les labels demandés et qu'il est dans le statut demandé.

- La casse ne compte ni pour les labels ni pour le statut : `equipe-a` trouve « Equipe-A ».
- Le statut doit correspondre en entier : « To do » ne prend pas « To do - QA ».
- Plusieurs labels s'écrivent séparés par des virgules, ou en répétant `--label`.

## Le fichier écrit

Un instantané de tout ce qui correspond au filtre à l'instant du passage, réécrit en entier puis renommé, pour qu'un lecteur ne voie jamais un fichier à moitié écrit.

```json
{
  "version": 1,
  "generatedAt": "2026-01-15T09:30:00.000Z",
  "tickets": [
    { "url": "https://gitlab.com/acme/shop/-/work_items/101", "title": "Corriger le total du panier", "source": "acme/shop" }
  ]
}
```

Un ticket qui ne correspond plus au filtre sort du fichier au passage suivant. Un projet qui ne répond pas garde ses tickets du fichier précédent.

Le fichier contient des titres de tickets : garde-le hors de tout dépôt.

## Fonctionnement

Sur macOS, `vigie start` installe une tâche `launchd` qui lance un passage par intervalle : rien ne reste en mémoire entre deux passages. `vigie stop` la retire. Ailleurs, ou avec `vigie start --resident`, un processus reste ouvert et dort entre deux passages (moins de 2 Mo).

Le journal est ramené à sa dernière moitié au-delà de 256 Ko.

La configuration, le journal et le pid sont dans `~/.config/vigie/` (`VIGIE_HOME` pour un autre dossier). Sans réglage `output`, le fichier est écrit dans ce dossier.

## Licence

MIT
