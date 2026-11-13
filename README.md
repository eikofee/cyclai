# CyclAI

Application **self-hosted, mono-utilisateur**, qui importe tes activités (vélo de route et course à pied : fichiers FIT/GPX, ou sources branchables comme Garmin Connect), calcule des métriques d'entraînement déterministes (TSS/IF, zones, CTL/ATL/TSB) et génère bilans et séances avec l'API Anthropic (Claude) — ou sans, via un « rapport IA » à coller dans n'importe quel assistant.

Écrite en Rust (axum + SQLite/sqlx), une seule image Docker, pas de JavaScript à builder. Prévue pour tourner sur un Raspberry Pi / NAS derrière un VPN : **il n'y a volontairement aucune authentification applicative**.

## Fonctionnalités

| | |
|---|---|
| **Sources** | Import de fichiers FIT / GPX (toujours disponible) + plugins d'activités (comme Garmin Connect) dans leur propre conteneur, avec priorité entre sources — voir [docs/PLUGINS.md](docs/PLUGINS.md) |
| **Métriques** | TSS / IF avec un FTP *manuel*, zones de puissance (7) et de FC (5), CTL / ATL / TSB (Banister), hrTSS pour la course |
| **Segments** | Segments importés depuis un fichier GPX ; matching GPS 100 % local (haversine), avec écarts en temps (et en watts s'il y a de la puissance) vs record / moyenne / dernier passage |
| **Bilan d'activité** | Texte qualitatif généré à la demande depuis la page d'une sortie, avec contexte libre optionnel |
| **Digest hebdo** | Séance structurée (JSON) proposée à partir de la charge des 4 dernières semaines |
| **Concepteur de séances** | Éditeur de séances par échelons (puissance, zones, FC, allure), profil graphique, TSS / durée / distance estimés |
| **Calendrier** | Sorties réalisées + séances planifiées (cliquer sur un jour pour planifier) |
| **Statistiques** | Distance totale vélo / course, volume par mois, toutes les traces sur une carte, records de vitesse sur distance, étoile de puissance par durée |
| **Itinéraire** | Charge un GPX : carte, profil d'altitude coloré par pente, D+ / D− |
| **API JSON** | `/api/v1/*` pour brancher un autre outil ou un agent — voir [API.md](API.md) |
| **Sans clé API** | Boutons « Télécharger le rapport IA » : la consigne + les données exactes, en markdown |

Détail de chaque fonctionnalité : [docs/FONCTIONNALITES.md](docs/FONCTIONNALITES.md).

## Démarrage rapide

Prérequis : Rust stable (édition 2024) ou Docker.

```bash
cp .env.example .env   # ou créer .env à la main, voir « Configuration »
cargo run -- serve     # dashboard sur http://localhost:3000
```

Au premier lancement la base `cyclai.db` est créée et les migrations appliquées. Tout se règle ensuite depuis **Paramètres** (`/settings`) : sources d'activités, clé Anthropic, FTP, FC, profil cycliste, prompts.

### Configuration initiale (`.env`)

Ces variables ne servent qu'à **amorcer** la base au tout premier démarrage ; ensuite `/settings` fait foi et l'environnement n'est plus relu.

| Variable | Défaut | Rôle |
|---|---|---|
| `DATABASE_URL` | `sqlite://cyclai.db` | Fichier SQLite |
| `CYCLAI_PLUGINS`, `CYCLAI_PLUGIN_<ID>_SECRET` | — | Plugins d'activités à interroger, relus à chaque démarrage (pas seulement le premier) — voir « Brancher une source d'activités » |
| `ANTHROPIC_API_KEY` | — | Optionnelle (sinon mode « rapport IA ») |
| `FTP_WATTS` | `200` | FTP **manuel** (jamais l'auto-FTP d'une source) |
| `HR_MAX`, `HR_REST` | `190`, `50` | Pour les zones Karvonen et le hrTSS |
| `ATHLETE_PROFILE` | — | Profil libre réinjecté dans chaque prompt LLM |

Aucune variable n'est obligatoire : sans rien, l'app démarre et se configure depuis le navigateur.

### Brancher une source d'activités

CyclAI fonctionne seul : le bouton **Importer GPX/FIT** du tableau de bord ne demande ni compte ni réseau. Pour récupérer automatiquement tes sorties depuis un service comme Garmin Connect, on ajoute un **plugin** : un petit service, dans son propre conteneur, qui détient les identifiants de la source. CyclAI l'appelle (requêtes signées) et ne lui donne aucun accès à sa base.

1. Ajoute le conteneur du plugin à ton `docker-compose.yml` (chaque plugin a son dépôt, par exemple [cyclai-garmin](../cyclai-garmin)).
2. Déclare-le pour CyclAI : `CYCLAI_PLUGINS=garmin=http://garmin:8080` et `CYCLAI_PLUGIN_GARMIN_SECRET=<secret partagé>`.
3. Dans **Paramètres > Sources d'activités**, active le plugin et règle l'ordre de priorité.
4. Dashboard → **Synchroniser**. **Tout l'historique** réimporte aussi les activités plus anciennes.

Le protocole est décrit dans [docs/PLUGINS.md](docs/PLUGINS.md).

## Commandes

| Commande | Rôle |
|---|---|
| `cyclai serve` | Dashboard web seul (sans API) sur `0.0.0.0:3000` (`--bind` / `BIND_ADDR`) |
| `cyclai serve-api` | API JSON seule sur `0.0.0.0:3001`, authentifiée par JWT RS256 — voir [API.md](API.md#authentification) |
| `cyclai api-key add\|list\|revoke\|token` | Gère les clés publiques autorisées à appeler l'API |
| `cyclai sync` | Récupère les nouvelles activités de chaque plugin activé, recalcule les métriques, relance le matching de segments |
| `cyclai sync --full` | Idem sur tout l'historique de chaque source (seules les activités absentes sont importées ; reprend après une limite de débit) |

Le sync ne génère **pas** de bilan LLM : un bilan se déclenche depuis la page de l'activité (voir [docs/FONCTIONNALITES.md](docs/FONCTIONNALITES.md#bilan-dactivité)), pour pouvoir ajouter du contexte avant de payer l'analyse.

## Déploiement

### Docker

```bash
docker build -t cyclai .
docker run -d --name cyclai -p 3000:3000 -v cyclai-data:/data cyclai
```

La base vit dans le volume `/data` (`DATABASE_URL=sqlite:///data/cyclai.db`). Les variables de l'amorçage ci-dessus peuvent être passées avec `-e` ou `--env-file`.

Sync quotidien depuis l'hôte (timer systemd, cron…) :

```bash
docker exec cyclai cyclai sync
```

Exemple de timer systemd (`/etc/systemd/system/cyclai-sync.{service,timer}`) :

```ini
# cyclai-sync.service
[Service]
Type=oneshot
ExecStart=/usr/bin/docker exec cyclai cyclai sync

# cyclai-sync.timer
[Timer]
OnCalendar=*-*-* 06:30:00
Persistent=true
[Install]
WantedBy=timers.target
```

`update.ps1` (local, ignoré par git) automatise le déploiement : build, export de l'image, envoi à Portainer puis redéploiement de la stack `cyclai` ; voir [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md#déploiement).

### Sécurité

Le dashboard (`serve`) n'a ni authentification ni TLS : n'expose jamais le port 3000 sur Internet, uniquement sur un réseau de confiance / VPN. La clé Anthropic est stockée **en clair** dans SQLite et affichée dans `/settings` ; les identifiants des sources vivent dans les conteneurs des plugins, pas dans la base de CyclAI.

Seule l'API (`serve-api`) est faite pour être exposée : elle ne sert rien d'autre que `/api/v1` et exige un JWT RS256 par requête. `docker-compose.yml` lance les deux processus sur la même base : l'UI dans le tailnet (`tailscale serve`), l'API éventuellement en funnel (`tailscale funnel`).

## Pages

| Route | Contenu |
|---|---|
| `/` | Charge 4 semaines (TSS, CTL/ATL/TSB, jauge de forme, zones), calendrier, activités récentes |
| `/activity/{id}` | Graphe superposé interactif, carte, laps, zones, courbe de puissance, segments, bilan |
| `/workouts`, `/workouts/{id}` | Digest hebdomadaire, modèles de séances et concepteur |
| `/planned/new`, `/planned/{id}` | Planification d'une séance sur un jour |
| `/stats` | Statistiques : totaux, traces sur la carte, records, radar de puissance |
| `/route` | Analyse d'un GPX d'itinéraire (rien n'est enregistré) |
| `/settings` | Clés, FTP / FC / allure seuil, profil, prompts système |
| `/api/v1/*` | API JSON ([API.md](API.md)) |

## Développement

```bash
cargo test            # 75 tests (calculs, parseurs FIT/GPX, API, migrations sur SQLite en mémoire)
cargo run -- serve    # pas de rechargement à chaud : relancer après modification
```

- Les requêtes utilisent `sqlx::query` / `query_as` à l'exécution (pas la macro `query!`) : aucune base n'est nécessaire à la compilation.
- Le schéma est une migration unique dans `migrations/` (pas de rétrocompatibilité : on la modifie directement et on recrée la base ; `db::adopt_squashed_history` ne sert qu'à reprendre une base antérieure au squash), embarquée au build (`sqlx::migrate!`) ; le `Dockerfile` force la recompilation quand elles changent.
- Les tests FIT s'appuient sur `tests/fixtures/*.fit`.
- Les LLM sont derrière le trait `LlmClient` : les tests utilisent un faux client, jamais l'API payante.

## Documentation

- [docs/FONCTIONNALITES.md](docs/FONCTIONNALITES.md) — comportement détaillé de chaque fonctionnalité
- [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) — modules, flux de données, schéma SQLite, décisions volontaires
- [API.md](API.md) — référence de l'API JSON
- [summary.md](summary.md) — état d'avancement et reste à faire

## Hors périmètre (volontaire)

- Connecteurs d'activités intégrés au cœur : ce sont des plugins externes (voir [docs/PLUGINS.md](docs/PLUGINS.md)). La création de séances dans un service tiers n'existe pas ; les séances se saisissent à la main depuis le plan généré.
- Authentification du dashboard (accès VPN uniquement) ; seule l'API exposée est authentifiée.
- `segment_efforts` calculés par une plateforme : tout le matching est local.
- Module salle de sport : retiré.
