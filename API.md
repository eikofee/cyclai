# API CyclAI

API JSON sous `/api/v1`, servie par un processus dédié : `cyclai serve-api` (port 3001 par défaut). Le dashboard (`cyclai serve`) ne sert **pas** l'API ; voir [Authentification](#authentification).

- Tout est en lecture seule, sauf la planification de séances (`POST` / `DELETE` sur `/planned`), la note du coach (`PUT /coach-note`) et les écritures d'un agent (bilan d'une sortie, rattachement sortie ↔ séance : voir [Agent](#agent-et-routine-quotidienne)).
- Réponses en JSON ; les erreurs ont la forme `{"error": "message"}` avec le code HTTP adapté (400, 401, 403, 404, 500).
- Dates au format `YYYY-MM-DD` ; `started_at` en ISO 8601 UTC.
- Unités : distances en km, durées en secondes (`duration_s`) ou minutes (`duration_min`), puissances en W, FC en bpm, dénivelé en m.

> Le même moteur est exposé aux agents via **MCP** (connecteur Claude, OAuth) : voir [`docs/MCP.md`](docs/MCP.md).

## Authentification

Chaque requête (y compris sur un chemin inconnu) doit porter `Authorization: Bearer <jwt>` : un JWT **RS256** que le client signe avec sa clé privée RSA (2048 bits minimum). Le serveur ne stocke que les clés publiques.

Claims et en-tête exigés :

| Champ | Valeur |
|---|---|
| en-tête `alg` | `RS256` (le seul accepté) |
| en-tête `kid` | nom de la clé enregistrée côté serveur |
| `iss` | le nom de la clé (identique à `kid`) |
| `aud` | `cyclai` |
| `iat` | date d'émission (epoch, secondes) |
| `exp` | expiration ; `exp - iat` ≤ **300 s** (tolérance d'horloge 30 s) |

Codes : `401` (jeton absent, invalide, expiré, clé inconnue ou révoquée — message unique `invalid token`), `403` (clé en lecture seule sur un `POST` / `DELETE`).

Gestion des clés (sur le serveur, ex. `docker exec -i cyclai-api cyclai …`) :

```bash
# côté client
openssl genrsa -out cyclai.key 3072
openssl rsa -in cyclai.key -pubout -out cyclai.pub

# côté serveur
cyclai api-key add laptop - --scope read < cyclai.pub    # ou --scope write
cyclai api-key list
cyclai api-key revoke laptop                              # effet immédiat

# test à la main (génère un jeton de 60 s)
TOKEN=$(cyclai api-key token laptop cyclai.key)
curl -H "Authorization: Bearer $TOKEN" http://localhost:3001/api/v1/profile
```

Exemple client Python (`pip install pyjwt cryptography`) :

```python
import time, jwt
now = int(time.time())
token = jwt.encode({"iss": "laptop", "aud": "cyclai", "iat": now, "exp": now + 60}, open("cyclai.key").read(),
                   algorithm="RS256", headers={"kid": "laptop"})
```

Les jetons ne sont pas à usage unique : sers l'API en HTTPS (ex. Tailscale Funnel) pour qu'un jeton ne puisse pas être rejoué par un tiers pendant sa courte validité.

Exposition avec Tailscale : [`deploy/tailscale-serve.json`](deploy/tailscale-serve.json) garde le dashboard privé sur `https://<machine>.<tailnet>.ts.net` (port 443, `AllowFunnel` à `false`) et publie l'API seule sur `https://<machine>.<tailnet>.ts.net:8443` (Funnel n'autorise que 443, 8443 et 10000). Le proxy cible `127.0.0.1:3001` : adapte-le au nom du conteneur (`cyclai-api:3001`) si Tailscale n'est pas dans le même espace réseau.

| Méthode | Chemin | Rôle |
|---|---|---|
| GET | [`/api/v1/profile`](#get-apiv1profile) | FTP, FC, zones, charge actuelle |
| GET | [`/api/v1/load`](#get-apiv1load) | Série quotidienne TSS / CTL / ATL / TSB |
| GET | [`/api/v1/activities`](#get-apiv1activities) | Dernières sorties |
| GET | [`/api/v1/activities/{id}`](#get-apiv1activitiesid) | Détail d'une sortie : laps détaillés, performances |
| GET | [`/api/v1/activities/{id}/streams`](#get-apiv1activitiesidstreams) | Séries temporelles (puissance, FC, allure…) |
| GET | [`/api/v1/summary`](#get-apiv1summary) | Synthèse des sorties récentes |
| GET | [`/api/v1/workouts`](#get-apiv1workouts) | Modèles de séance et catalogue (Zwift…), filtrables |
| GET | [`/api/v1/workouts/{id}`](#get-apiv1workoutsid) | Un modèle avec toutes ses étapes |
| GET | [`/api/v1/workouts/categories`](#get-apiv1workoutscategories) | Sommaire du catalogue : catégories, plans, effectifs |
| POST | [`/api/v1/proposals`](#post-apiv1proposals) | Suggérer une séance (à valider dans le dashboard) |
| POST | [`/api/v1/planned/{id}/skip`](#post-apiv1plannedidskip) | Marquer une séance comme passée |
| POST | [`/api/v1/projection`](#post-apiv1projection) | Simuler la charge et contrôler les règles |
| GET | [`/api/v1/rules`](#règles-dentraînement-et-projection-de-charge) | Contraintes appliquées par le serveur |
| GET / PUT | [`/api/v1/checkins`](#check-ins) | Ressenti quotidien |
| GET / PUT | [`/api/v1/availability`](#disponibilités) | Temps disponible par jour (règles + exceptions) |
| GET | [`/api/v1/ride-envelope`](#get-apiv1ride-envelope) | Sorties possibles un jour donné, TSS typique et faisabilité |
| GET | [`/api/v1/coach-note`](#coach-note) | Message affiché en haut du dashboard |
| PUT | [`/api/v1/coach-note`](#coach-note) | Remplacer (ou effacer) ce message |
| GET | [`/api/v1/briefing`](#agent-et-routine-quotidienne) | Tout pour le point du jour en un appel |
| GET | [`/api/v1/power-bests`](#agent-et-routine-quotidienne) | Meilleures puissances par durée (fenêtre, fenêtre précédente, tout temps) |
| PUT | [`/api/v1/activities/{id}/bilan`](#agent-et-routine-quotidienne) | Écrire / remplacer le bilan d'une sortie |
| POST | [`/api/v1/planned/{id}/link`](#agent-et-routine-quotidienne) | Rattacher une sortie à une séance planifiée |
| GET | [`/api/v1/planned`](#get-apiv1planned) | Séances planifiées |
| POST | [`/api/v1/planned`](#post-apiv1planned) | Planifier une séance |
| DELETE | [`/api/v1/planned/{id}`](#delete-apiv1plannedid) | Retirer une séance du calendrier |

---

## GET /api/v1/profile

Profil physiologique, zones dérivées et charge d'entraînement actuelle.

Pas de paramètre.

```json
{
  "ftp_watts": 212,
  "hr_max": 192,
  "hr_rest": 53,
  "hr_threshold_bpm": 167,
  "hr_tss_coefficient": 1.0,
  "run_threshold_pace": "4:30",
  "run_threshold_pace_s_per_km": 270.0,
  "power_zones": [
    {"zone": 1, "name": "Récupération active", "min_w": 0, "max_w": 117},
    {"zone": 7, "name": "Neuromusculaire", "min_w": 318, "max_w": null}
  ],
  "hr_zones": [
    {"zone": 1, "name": "Récupération", "min_bpm": 53, "max_bpm": 136},
    {"zone": 5, "name": "VO2max/Anaérobie", "min_bpm": 178, "max_bpm": null}
  ],
  "load": {
    "ctl": 41.9,
    "atl": 55.1,
    "tsb": -13.2,
    "tsb_status": "Fatigue modérée, entraînement productif",
    "rest_days_to_next_tsb_band": 1,
    "tss_28d": 1531,
    "power_zone_seconds_28d": [45209, 19319, 14713, 10789, 6326, 5149, 2999],
    "power_zone_pct_28d": [43.3, 18.5, 14.1, 10.3, 6.1, 4.9, 2.9]
  }
}
```

- 7 zones de puissance (Coggan étendu, en % du FTP) ; 5 zones de FC (Karvonen). `max_*` vaut `null` pour la dernière zone.
- `rest_days_to_next_tsb_band` : jours de repos pour passer dans la tranche de TSB supérieure ; `null` dans la tranche la plus haute ou si inatteignable.
- `hr_threshold_bpm` : FC seuil (LTHR) utilisée pour le hrTSS — celle réglée dans `/settings`, ou FC repos + 85 % de la réserve cardiaque si elle n'est pas renseignée.
- `run_threshold_pace` / `run_threshold_pace_s_per_km` : allure seuil en course à pied (réglée dans `/settings`), `null` si non renseignée. Référence des cibles d'allure des séances.
- `hr_tss_coefficient` : coefficient appliqué au hrTSS des courses quand il s'additionne au TSS puissance du vélo (réglable dans `/settings`, 1 = compte pareil, 0 = les courses ne comptent pas).
- La charge (CTL / ATL / TSB, `tss_28d`) cumule le TSS puissance du vélo et le **hrTSS** des courses à pied **pondéré par ce coefficient**.
- `power_zone_*_28d` : temps en zones de puissance, vélo uniquement (une course n'a pas de zones de puissance).

## GET /api/v1/load

Série quotidienne de charge, du plus ancien au plus récent.

| Paramètre | Défaut | Description |
|---|---|---|
| `days` | `90` | Nombre de jours jusqu'à aujourd'hui (1 à 1500) |

```json
{
  "days": 3,
  "series": [
    {"date": "2026-10-04", "tss": 254.8, "ctl": 44.0, "atl": 75.1, "tsb": -31.0},
    {"date": "2026-10-05", "tss": 0.0, "ctl": 43.0, "atl": 64.3, "tsb": -21.4}
  ]
}
```

## GET /api/v1/activities

Dernières sorties, de la plus récente à la plus ancienne.

| Paramètre | Défaut | Description |
|---|---|---|
| `limit` | `10` | Nombre max de résultats (1 à 200) |
| `days` | — | Ne garder que les N derniers jours (sans valeur : tout l'historique) |
| `sport` | — | `ride` ou `run` ; autre valeur ou absent : les deux |

```json
{
  "count": 1,
  "activities": [{
    "id": 20449027159,
    "sport": "ride",
    "sport_type": "Ride",
    "indoor": false,
    "power_source": "pedals",
    "has_hr": true,
    "started_at": "2026-10-04T09:52:00Z",
    "title": "Sortie du dimanche",
    "distance_km": 95.3,
    "duration_s": 14760,
    "elevation_gain_m": 1490,
    "avg_pace_min_per_km": null,
    "avg_speed_kmh": 23.2,
    "avg_power_w": 152,
    "normalized_power_w": 167,
    "avg_hr": 156,
    "max_hr": 178,
    "tss": 255,
    "tss_source": "power",
    "tss_load": 255,
    "intensity_factor": 0.79,
    "power_zone_seconds": [4800, 3200, 3300, 2200, 900, 300, 60]
  }]
}
```

- `sport` vaut `ride` ou `run`. Pour une course : `avg_pace_min_per_km` (ex. `"5:08"`) est renseigné, `avg_speed_kmh` est `null` ; l'inverse pour le vélo.
- `tss_source` : `power` (vélo, basé sur la puissance normalisée et le FTP) ou `hr` (course à pied : **hrTSS**, basé sur la FC — charge TRIMP rapportée à une heure à la FC seuil = 100). Un TSS vaut `null` si la donnée manque (vélo sans capteur de puissance, course sans FC).
- `tss_load` : ce que la sortie ajoute à la charge totale — égal à `tss` pour le vélo, `tss` × `hr_tss_coefficient` pour une course. Le `tss` d'une course reste son hrTSS brut.
- Les champs de puissance et `intensity_factor` sont `null` quand la donnée n'existe pas (toujours le cas pour une course).
- Les sorties importées depuis un fichier FIT/GPX ont un `id` négatif.
- Champs dérivés : `indoor` (`true` pour une sortie `VirtualRide`, trainer/Zwift), `power_source` (`none` sans puissance, `trainer` en intérieur, `pedals` en extérieur — les sources ne distinguent pas plus finement), `has_hr` (une FC moyenne existe).

## GET /api/v1/activities/{id}

Une sortie, avec les mêmes champs que ci-dessus plus le détail calculé à partir des flux bruts (puissance, FC, cadence, altitude, distance échantillonnés à la seconde).

```json
{
  "laps": [{
    "index": 2, "start_s": 2259, "duration_s": 96,
    "distance_km": 0.54, "avg_speed_kmh": 20.1, "avg_pace_min_per_km": null,
    "avg_power_w": 184, "max_power_w": 252, "normalized_power_w": 194, "intensity_factor": 0.92,
    "avg_hr": 151, "max_hr": 157,
    "avg_cadence_rpm": 88, "avg_cadence_spm": null,
    "elevation_gain_m": 17, "avg_grade_pct": 3.1,
    "power_zone_seconds": [3, 23, 32, 17, 21, 0, 0],
    "hr_zone_seconds": [0, 33, 63, 0, 0]
  }],
  "performance": {
    "duration_s": 17081,
    "avg_power_w": 132, "max_power_w": 560, "normalized_power_w": 167,
    "variability_index": 1.27, "intensity_factor": 0.79,
    "power_curve_w": {"5s": 524, "15s": 389, "30s": 370, "1min": 330, "5min": 222,
                      "10min": 217, "20min": 196, "60min": 165},
    "avg_hr": 155, "max_hr": 190,
    "hr_zone_seconds": [2069, 4315, 4290, 6168, 239],
    "avg_cadence_rpm": 65, "avg_cadence_spm": null,
    "avg_grade_pct": 0.0,
    "power_hr_decoupling_pct": 4.2
  },
  "segment_efforts": [
    {"segment": "Mont Saint-Quentin", "avg_power_w": 220,
     "delta_vs_pr_pct": -3.2, "delta_vs_recent_avg_pct": 4.1, "delta_vs_last_pct": 1.5}
  ],
  "bilan": "Texte du bilan généré, ou null"
}
```

**Laps** — chaque lap est découpé dans les flux à partir de sa position dans la sortie (`start_s` = secondes écoulées depuis le départ) :
- `avg_power_w` / `max_power_w` / `normalized_power_w` (NP sur 30 s glissantes, `null` si le lap dure moins de 30 s) et `intensity_factor` (NP / FTP) ;
- `avg_hr` / `max_hr`, cadence (`avg_cadence_rpm` pour le vélo, `avg_cadence_spm` en pas/min pour la course), D+, pente moyenne, vitesse ou allure ;
- `power_zone_seconds` (7 zones, vélo) et `hr_zone_seconds` (5 zones Karvonen) : temps passé dans chaque zone.
- Sans flux disponibles, un lap retombe sur les valeurs résumées de la source (distance, puissance et FC moyennes) et les autres champs valent `null`.

**Performance** (`null` sans flux) — métriques sur toute la sortie :
- `power_curve_w` : meilleure puissance moyenne sur 5 s à 60 min (`null` si la sortie est plus courte) ;
- `variability_index` : NP / puissance moyenne ;
- `power_hr_decoupling_pct` : baisse du rapport puissance/FC entre la 1ʳᵉ et la 2ᵉ moitié (positif = la FC dérive à puissance égale) ; seulement pour les efforts d'au moins 20 min avec puissance et FC ;
- pour une course : pas de champs de puissance, cadence en `avg_cadence_spm`, allure sur les laps (`avg_pace_min_per_km`).

`segment_efforts` : efforts sur les segments de référence (matching local), vide pour une course. `404` si l'activité n'existe pas.

## GET /api/v1/activities/{id}/streams

Séries temporelles d'une sortie, moyennées par tranches de `step` secondes.

| Paramètre | Défaut | Description |
|---|---|---|
| `step` | `10` | Taille des tranches en secondes (1 à 300) |
| `from_s` | `0` | Début de la fenêtre (secondes depuis le départ) |
| `to_s` | fin de la sortie | Fin de la fenêtre (exclue) |

Pour zoomer sur un lap : `from_s=<start_s>&to_s=<start_s + duration_s>`.

```json
{
  "activity_id": 20449027159, "step_s": 300, "from_s": 0, "count": 57,
  "time_s": [0, 300, 600],
  "distance_km": [0.0, 1.9, 4.1],
  "speed_kmh": [22.2, 29.4, 26.7],
  "pace_min_per_km": null,
  "power_w": [127, 169, 147],
  "hr_bpm": [136, 150, 142],
  "cadence": [80, 85, 83],
  "altitude_m": [210.4, 230.1, 245.7]
}
```

- Une série absente de la sortie vaut `null`. Pour une course : `pace_min_per_km` (allure décimale, `null` à l'arrêt) remplace `speed_kmh`, et il n'y a pas de puissance.
- `404` si l'activité n'existe pas ou n'a pas de flux ; `400` si la fenêtre est vide ou hors de la sortie.

## GET /api/v1/summary

Synthèse de l'entraînement récent.

| Paramètre | Défaut | Description |
|---|---|---|
| `days` | `28` | Fenêtre en jours (1 à 3650) |

```json
{
  "days": 28,
  "since": "2026-09-09",
  "totals": {"activities": 11, "distance_km": 645.2, "duration_s": 96000,
             "elevation_gain_m": 6651, "tss": 1531, "avg_intensity_factor": 0.78},
  "by_sport": {"Ride": {"activities": 10, "...": "..."}, "Run": {"activities": 1, "...": "..."}},
  "by_week": [
    {"week_start": "2026-09-07", "activities": 4, "distance_km": 215.6,
     "duration_s": 32696, "elevation_gain_m": 2251, "tss": 501}
  ],
  "latest_activity": { "...": "même format que /activities" }
}
```

- `by_sport` est indexé par `sport_type` (`Ride`, `Run`, `TrailRun`). Les `tss` (totaux, par sport, par semaine) additionnent le TSS puissance (vélo) et le hrTSS (course) **pondéré par le coefficient hrTSS**, comme la charge.
- `by_week` : semaines débutant le lundi.

## GET /api/v1/workouts

Les modèles de séance réutilisables : créés dans le concepteur, générés par le digest, ou importés du catalogue (ex. plans Zwift, `category` = `Depuis Zwift`). Utilise un `id` comme `workout_id` pour planifier.

Paramètres (tous facultatifs, combinables) :

| Paramètre | Description |
|---|---|
| `category` | Catégorie du catalogue (`Depuis Zwift`…). Voir `/workouts/categories` |
| `group` | Plan au sein de la catégorie (`group_key`, ex. `build-me-up`) |
| `source` | Origine exacte : `manual`, `digest`… |
| `sport` | `cycling`, `running` ou `other` |
| `q` | Sous-chaîne du nom (insensible à la casse) |
| `min_duration_min`, `max_duration_min` | Bornes de durée estimée |
| `min_tss`, `max_tss` | Bornes de TSS estimé (au FTP courant) |
| `min_if`, `max_if` | Bornes d'intensity factor (calculé au FTP courant) |
| `min_zone_time` | Temps minimal par zone de puissance, `zone:minutes[,zone:minutes]` (zones 1–7) : `4:20` = au moins 20 min en Z4, `4:10,5:5` = les deux conditions. `400` si mal formé |
| `tag` | Type de séance (voir plus bas) : `recuperation`, `endurance`, `tempo`, `seuil`, `vo2`, `anaerobie`. Plusieurs valeurs séparées par des virgules = l'une d'elles |
| `target_tss` | Trie par proximité de ce TSS (le plus proche d'abord) au lieu de l'ordre habituel |
| `exclude_done_within_days` | Écarte les modèles déjà réalisés (séance planifiée `done`) dans les N derniers jours |
| `steps` | `true` pour inclure la structure complète (`workout`). Absent par défaut : le catalogue est volumineux |
| `limit` / `offset` | Pagination (`limit` défaut 200, max 500) |

Avec `category` ou `group`, l'ordre est celui du plan (`position`) ; sinon du plus récent au plus ancien.

```json
{
  "total": 1, "count": 1,
  "workouts": [{
    "id": 4,
    "name": "Over-under 2x12",
    "source": "manual",
    "sport": "cycling",
    "category": null, "group_key": null, "group_name": null, "subgroup": null,
    "position": 0,
    "description": null, "source_url": null,
    "duration_min": 55.0,
    "estimated_tss": 62,
    "intensity_factor": 0.88,
    "power_zone_minutes": [5.0, 3.0, 10.0, 24.0, 0.0, 0.0, 0.0],
    "tags": ["seuil"],
    "plan": null,
    "last_done": "2026-10-28",
    "computed_at_ftp_w": 205,
    "estimated_distance_km": 24.5
  }]
}
```

Pour une séance du catalogue : `group_key` / `group_name` identifient le plan (ex. `ftp-builder` / « FTP Builder »), `subgroup` la semaine, `position` l'ordre d'origine, `source_url` la page d'origine. `estimated_tss` est une estimation à partir des puissances cibles moyennes.

Champs **calculés à la volée au FTP courant** (`computed_at_ftp_w`), jamais figés : `intensity_factor`, `power_zone_minutes` (minutes en Z1…Z7 ; les rampes sont réparties sur les zones traversées) et `tags`, déduits de la répartition : `recuperation` (IF < 0,55), `endurance` (≥ 70 % en Z1–Z2 et IF < 0,75), `tempo` (≥ 15 min en Z3), `seuil` (≥ 10 min en Z4), `vo2` (≥ 5 min en Z5), `anaerobie` (≥ 2 min en Z6–Z7). Une séance peut en cumuler plusieurs. `plan` (`null` hors catalogue) regroupe `category`, `group_key`, `group_name`, `subgroup`, `position` : le plan d'origine. `last_done` est la date de la dernière séance planifiée réalisée à partir de ce modèle (`null` sinon).

## GET /api/v1/workouts/{id}

Un modèle, mêmes champs que ci-dessus plus `workout` (`{"name": ..., "steps": [...]}`, format décrit sous `POST /planned`). `404` si inconnu.

## GET /api/v1/workouts/categories

Sommaire du catalogue :

```json
{"categories": [{"category": "Depuis Zwift", "groups": [
  {"group_key": "ftp-builder", "group_name": "FTP Builder", "count": 24}]}]}
```

## Agent et routine quotidienne

Ces routes permettent à un agent IA externe de piloter le point quotidien (CyclAI ne lance rien lui-même). Toutes sauf `GET` demandent une clé `write` (ou le scope MCP `cyclai:propose`).

- `GET /api/v1/briefing` : rapproche d'abord les sorties des séances (comme `GET /planned`), puis renvoie en un appel `today`, `profile` (FTP, zones, forme), `load` (14 j), `recent_activities` (7 j, avec `planned_id` et `has_bilan`), `activities_without_bilan` (21 j), `planned` (de J-7 à J+7), `missed_sessions` (séances passées jamais rattachées à une sortie), `proposals_awaiting_validation`, `power_bests` (42 j), `checkins` (7 j), `availability` (7 j) et `coach_note`.
- `GET /api/v1/power-bests?days=42` (7 à 730) : pour chaque durée de référence (15 s à 60 min), `window` (meilleure puissance moyenne sur les `days` derniers jours), `prior_window` (la fenêtre de même longueur juste avant, pour la tendance) et `all_time`, chacune avec `watts`, `pct_ftp`, `activity_id`, `date`.
- `GET /api/v1/activities?unreviewed=true` ne garde que les sorties sans bilan ; chaque sortie des listes porte `planned_id` et `has_bilan`. `GET /activities/{id}` ajoute `bilan_author`, `bilan_at` et `planned` : la séance planifiée réalisée, avec `comparison` (durée, TSS, IF prévus vs réels), ou `null` si la sortie n'était pas prévue.
- `PUT /api/v1/activities/{id}/bilan` avec `{"text": "...", "author": "coach"}` : écrit le bilan (1 à 1500 caractères, remplace l'existant, comme « Régénérer le bilan »). `author` est facultatif (60 car. max) ; `404` si la sortie n'existe pas.
- `POST /api/v1/planned/{id}/link` avec `{"activity_id": 42}` : pour une séance que le rapprochement automatique (même jour, même sport) n'a pas appariée, par exemple faite avec un jour de retard. La séance (`planned`, `confirmed` ou `proposed`) passe à `done`. `409` si l'activité est déjà rattachée ou si le statut ne le permet pas. Rien ne se supprime.
- Le message du jour (MotD) reste `PUT /api/v1/coach-note`.

## Coach note

Un court message libre (1000 caractères max, texte brut) affiché en bandeau en haut du dashboard, pour qu'un agent laisse une consigne à l'utilisateur (« semaine douce après la sortie du week-end »). Une seule note à la fois ; l'utilisateur peut l'effacer depuis le dashboard.

- `GET /api/v1/coach-note` → `{"message": "...", "author": "coach", "updated_at": "2026-11-10 08:00:00"}` ou `{"message": null}`.
- `PUT /api/v1/coach-note` (clé `write`) avec `{"message": "...", "author": "coach"}` remplace la note ; `author` est facultatif ; un `message` vide l'efface. `400` si plus de 1000 caractères.

## Règles d'entraînement et projection de charge

Le serveur applique des contraintes dures, réglables dans `/settings` (« Règles d'entraînement ») : un agent les voit dans les réponses mais ne peut pas les contourner.

| Règle (`rule`) | Défaut | Violation si… |
|---|---|---|
| `consecutive_intense` | 2 | plus de N jours intenses de suite |
| `no_rest_day` | 1 | moins de N jours de repos (TSS du jour < 10) sur 7 jours glissants |
| `ctl_ramp` | +6 | le CTL gagne plus de N points en 7 jours |
| `tsb_floor` | −25 | le TSB passe sous N |

Un jour est **intense** si son IF atteint 0,85 ou si son TSS atteint 100 (réglable aussi).

`GET /api/v1/rules` renvoie les valeurs en vigueur.

### POST /api/v1/projection

Simulation, **rien n'est écrit** (accessible avec une clé `read`). Projette CTL/ATL/TSB jour par jour avec les mêmes constantes que le dashboard, en comptant les séances déjà au calendrier (statuts `planned` et `confirmed`) plus celles que tu ajoutes.

```json
{"sessions": [
  {"date": "2026-11-14", "workout_id": 12},
  {"date": "2026-11-15", "tss": 150, "intensity_factor": 0.9}
], "include_planned": true}
```

Chaque séance donne soit `workout_id` (TSS et IF calculés au FTP courant), soit `tss` (avec `intensity_factor` facultatif). Les dates vont d'aujourd'hui à 120 jours (une séance du jour s'ajoute à la charge déjà roulée, en supposant les sources synchronisées ; si une sortie est déjà enregistrée aujourd'hui, la séance planifiée du jour n'est pas recomptée). Réponse :

```json
{"from": "2026-11-10", "ok": false,
 "days": [{"date": "2026-11-11", "tss": 0, "ctl": 52.1, "atl": 48.0, "tsb": 4.1, "intense": false}],
 "violations": [{"rule": "consecutive_intense", "date": "2026-11-16", "message": "3 jours intenses de suite (max 2)"}],
 "rules": {"...": "..."}}
```

Une violation n'est signalée qu'une fois par série (le premier jour fautif).

## Check-ins

`PUT /api/v1/checkins/{date}` (clé `write`) enregistre le ressenti du jour, un par date (le suivant le remplace) : `{"legs": 1-5, "motivation": 1-5, "rhr_bpm": 48, "note": "...", "weather": "..."}` (`rhr_bpm`, `note` et `weather` facultatifs). `GET /api/v1/checkins?from=&to=` les liste, du plus récent au plus ancien (30 derniers jours par défaut).

## Disponibilités

Temps disponible pour rouler, jour par jour : une règle par défaut selon le jour de la semaine (réglable dans `/settings`, par défaut 90 min en soirée du lundi au vendredi, 180 min en journée le week-end), remplacée par une exception à la date quand il y en a une.

`GET /api/v1/availability?from=&to=` (14 jours à partir d'aujourd'hui par défaut, 92 jours au plus) :

```json
{"from": "2026-11-10", "to": "2026-11-11", "count": 2, "days": [
  {"date": "2026-11-10", "available_min": 90, "time_of_day": "soir", "context": "travail", "source": "regle_par_defaut", "note": null},
  {"date": "2026-11-11", "available_min": 300, "time_of_day": "journee", "context": "vacances", "source": "exception", "note": "congés"}
]}
```

`PUT /api/v1/availability` (clé `write`) crée ou remplace l'**exception** d'un jour (`{"date": "2026-11-11", ...}`) ou d'une plage (`{"from": "...", "to": "...", ...}`, 92 jours au plus, bornes incluses), avec `available_min` (0 à 720), `time_of_day` (`matin` | `soir` | `journee`), `context` (`travail` | `vacances` | `autre`) et une `note` facultative (300 caractères). Les dates vont d'aujourd'hui à plus tard (`400` sinon). Les règles par défaut ne sont modifiables que depuis l'interface, et il n'existe pas de suppression par l'API (les exceptions se suppriment dans `/settings`).

## GET /api/v1/ride-envelope

Pour un jour et le temps disponible, les types de sortie possibles, ce qu'ils coûtent d'habitude en TSS et ce que les règles en disent. Lecture seule, aucune écriture.

| Paramètre | Défaut | Description |
|---|---|---|
| `date` | obligatoire | `YYYY-MM-DD`, aujourd'hui ou plus tard (120 jours au plus) |
| `available_min` | disponibilité du jour | Temps pour rouler, de 0 à 720 |

```json
{"date": "2026-11-14", "available_min": 180, "available_source": "regle_par_defaut",
 "time_of_day": "journee", "context": "autre", "slot_type": "journee",
 "scenarios": [
  {"scenario": "moyenne", "intent": "endurance", "duration_min": 145, "tss_range": [74, 96],
   "tss_source": "history", "sample_size": 6, "source": "computed",
   "ctl_after": 52.4, "tsb_after": -9.8, "feasibility": "ok", "rule": null},
  {"scenario": "longue_denivele", "intent": "denivele", "duration_min": 180, "tss_range": [130, 216],
   "tss_source": "engine_default", "sample_size": 1, "source": "estimated",
   "ctl_after": 54.1, "tsb_after": -27.3, "feasibility": "deconseille",
   "rule": {"rule": "tsb_floor", "date": "2026-11-14", "message": "TSB -27.3 sous le plancher de -25.0"}}
 ]}
```

- **Scénarios** : `tranquille` (la moitié du temps, dès 30 min), `moyenne` (80 % du temps, dès 60 min) et `longue_denivele` (tout le temps, dès 150 min). Seuls ceux qui tiennent dans le temps disponible sont renvoyés. `slot_type` : `soiree_courte` (jusqu'à 90 min), `soiree_longue` (jusqu'à 150 min) ou `journee`.
- **`tss_range`** : le quart central (25ᵉ à 75ᵉ percentile) du TSS de tes sorties à vélo des 12 derniers mois dont la durée est proche (±25 %) ; pour `longue_denivele`, seulement celles qui grimpent au moins 250 m par heure. Il faut au moins 3 sorties (`sample_size`), sinon la fourchette vient du moteur (60 % à 100 % du plafond de l'intention) : `tss_source` vaut alors `engine_default` et `source` `estimated` au lieu de `history` et `computed`. Le haut est borné par ce que l'intention permet sur la durée.
- **`ctl_after`, `tsb_after`** : projetés le jour même, pour une sortie au milieu de la fourchette, avec le calendrier (`planned` et `confirmed`).
- **`feasibility`** : `deconseille` si la sortie au milieu de la fourchette viole une règle dure qu'elle n'enfreignait pas déjà, `limite` si seul le haut de la fourchette la viole, `ok` sinon. `rule` donne la première violation qui l'explique.
- Les séances déjà planifiées ce jour-là sont ignorées : l'enveloppe décrit ce qui pourrait les remplacer.

## Propositions et statuts de séance

Cycle de vie (`status`) : `planned` (créée à la main ou par `POST /planned`), `proposed` (suggérée par un agent), `confirmed`, `done`, `skipped`, `rejected`. `done`, `skipped` et `rejected` sont définitifs. Seules `planned` et `confirmed` comptent dans la projection de charge ; `skipped` et `rejected` disparaissent du calendrier du dashboard (elles restent en base et dans l'API), et une `proposed` y apparaît en pointillés avec un « ? ».

### POST /api/v1/proposals

Clé `write`. Suggère une séance à partir d'un modèle ; l'utilisateur la **confirme ou la refuse** depuis sa page dans le dashboard (un agent ne peut pas valider lui-même).

```json
{"date": "2026-11-14", "workout_id": 12, "rationale": "Séance clé de la semaine, TSB à +8", "idempotency_key": "w46-sat"}
```

- `201` avec la séance (`status: "proposed"`, `rationale`, `estimated_tss`, `estimated_if`…).
- `200` avec la séance déjà créée si la même `idempotency_key` est rejouée.
- `422` si la séance introduirait une violation des [règles](#règles-dentraînement-et-projection-de-charge) qui n'existait pas déjà (le message liste les règles, avec leur date) : rien n'est enregistré. Une violation déjà présente dans le calendrier n'est pas imputée à la proposition.
- `400` : date antérieure à aujourd'hui ou à plus de 120 jours, `rationale` vide ou de plus de 500 caractères, clé de plus de 100 caractères ; `404` : modèle inconnu.

La date peut être aujourd'hui : la séance s'ajoute alors à la charge déjà roulée (sources supposées synchronisées).

#### Alternatives

`alternatives` (facultatif, avec `workout_id` uniquement) offre jusqu'à 3 autres séances de la bibliothèque entre lesquelles l'utilisateur peut choisir :

```json
{"date": "2026-11-14", "workout_id": 12, "rationale": "Séance clé", "alternatives": [
  {"workout_id": 31, "note": "plus doux si les jambes sont lourdes"}, {"workout_id": 40}]}
```

- Chaque alternative est vérifiée comme la séance principale : si l'une enfreindrait une règle dure, toute la requête est refusée (`422`, `alternative 31 refused…`) et rien n'est enregistré. `400` : plus de 3, doublons, identique à la séance principale, `note` de plus de 200 caractères, ou combinées à `free_ride_budget` ; `404` : modèle inconnu.
- La séance renvoyée porte `alternatives` : `[{workout_id, name, duration_min, estimated_tss, note}]` (instantané au moment de la proposition).
- Dans le dashboard, la page de la proposition liste ces options ; « Prendre celle-ci à la place » remplace la séance (l'ancienne devient une alternative), tant qu'elle n'est pas confirmée.

#### Sorties libres

À la place de `workout_id`, `free_ride_budget` propose une sortie sans structure imposée, bornée par un budget (donnez l'un ou l'autre, jamais les deux : `400`) :

```json
{"date": "2026-11-15", "rationale": "Sortie longue tranquille", "idempotency_key": "w46-sun",
 "free_ride_budget": {"duration_max_min": 150, "intent": "endurance", "tss_range": [70, 110]}}
```

- `intent` : `endurance` | `denivele` | `recuperation` | `tempo` | `intense` | `course` (plafonds IF 0,75 / 0,85 / 0,60 / 0,88 / 0,95 / 1,0). `expected_if` (0,3 à 1,2, facultatif) donne l'intensité réelle attendue (course, sortie de groupe rapide, test) : il remplace le plafond de l'intention, le budget est construit autour (TSS = durée × IF², ±10 %) et c'est cet IF que voient les règles dures (jours intenses consécutifs). Incompatible avec `tss_range`. `tss_range` est facultatif : sans lui, le moteur le calcule à partir de la durée et de l'intention (60 % à 100 % du plafond).
- Le budget est borné par le moteur (`422` sinon) : `duration_max_min` (20 minimum) ne peut pas dépasser le temps disponible ce jour-là ([disponibilités](#disponibilités)), et le TSS maximal ne peut pas dépasser ce que l'intention permet sur cette durée (IF plafond 0,75 en endurance, 0,85 en dénivelé, 0,60 en récupération ; soit environ 56, 72 et 36 TSS par heure).
- Règles dures : la proposition est vérifiée avec le haut du budget (pire cas). Une fois confirmée, la sortie compte dans la projection au milieu de la fourchette.
- La séance renvoyée a `kind: "free_ride"` et un objet `free_ride_budget` (`tss_range`, `duration_max_min`, `intent`) ; les séances de bibliothèque ont `kind: "workout"`.

#### Réalisation automatique

Une séance `planned` ou `confirmed` dont le jour est arrivé passe seule en `done` dès qu'une sortie du même sport existe ce jour-là (vélo : `Ride` ou `VirtualRide` ; course : `Run` ou `TrailRun`). Chaque sortie ne correspond qu'à une séance, et `activity_id` indique laquelle. Le rapprochement se fait à chaque lecture du calendrier ou de la projection ; une séance non réalisée reste `planned` jusqu'à ce qu'on la passe (`skip`). La charge réelle remplace alors l'estimation dans la projection.

### POST /api/v1/planned/{id}/skip

Clé `write`. Passe une séance à venir à `skipped` ; corps `{"reason": "jambes lourdes"}` (facultatif, ajouté aux notes). `409` si le statut est définitif, `404` si elle n'existe pas.

Les séances planifiées portent maintenant `status` (`planned`, `proposed`, `confirmed`, `done`, `skipped`, `rejected`), `rationale`, `estimated_if`, `kind`, `free_ride_budget` et `activity_id`.

## GET /api/v1/planned

Séances planifiées dans l'intervalle `from` ≤ date < `to`, triées par date.

| Paramètre | Défaut | Description |
|---|---|---|
| `from` | aujourd'hui − 7 j | Date de début (incluse) |
| `to` | aujourd'hui + 42 j | Date de fin (exclue) |
| `sport` | — | `cycling`, `running` ou `other` ; absent : tous |

```json
{
  "from": "2026-09-29",
  "to": "2026-11-17",
  "count": 1,
  "planned": [{
    "id": 1,
    "date": "2026-10-09",
    "name": "Sweet spot 2x20",
    "notes": "via api",
    "workout_id": null,
    "source": "api",
    "sport": "cycling",
    "duration_min": 65.0,
    "estimated_tss": 68,
    "estimated_distance_km": 30.7,
    "workout": {"name": "Sweet spot 2x20", "steps": [ "..." ]}
  }]
}
```

- `workout_id` : modèle d'origine, `null` si la séance est unique à son jour ou si le modèle a été supprimé. Une séance planifiée possède sa propre copie des étapes : modifier ou supprimer le modèle ne la change pas.
- `source` : `manual` (interface web) ou `api`.
- `sport` : `cycling`, `running` ou `other`. L'estimation de distance (`estimated_distance_km`) suit le sport : modèle de vitesse pour le cyclisme, allures des étapes pour la course (`null` si l'allure seuil manque pour une étape sans allure), `null` pour « autre ».
- `400` si `from` / `to` ne sont pas des dates `YYYY-MM-DD` ou si `sport` est inconnu.

## POST /api/v1/planned

Planifie une séance sur un jour. Plusieurs séances par jour sont possibles. Réponse : `201` avec la séance créée (même format qu'un élément de `GET /planned`).

Corps JSON (`Content-Type: application/json`) :

| Champ | Obligatoire | Description |
|---|---|---|
| `date` | oui | `YYYY-MM-DD` |
| `workout_id` | un des deux | Copie les étapes du modèle (voir `GET /workouts`) |
| `workout` | un des deux | Séance inline `{"name": "...", "steps": [...]}` (format ci-dessous) |
| `name` | non | Remplace le nom de la séance (pratique avec `workout_id`) |
| `notes` | non | Note libre |
| `sport` | non | `cycling` (défaut), `running` ou `other`. Avec `workout_id`, reprend par défaut le sport du modèle |

Il faut exactement un de `workout_id` ou `workout`.

Exemple :

```bash
curl -X POST http://localhost:3001/api/v1/planned \
  -H "Authorization: Bearer $TOKEN" \
  -H 'content-type: application/json' \
  -d '{
    "date": "2026-10-12",
    "notes": "avant la sortie club",
    "workout": {"name": "Sweet spot 2x20", "steps": [
      {"type": "warmup", "duration": {"type": "time", "value": "15min"},
       "target": {"type": "power_range", "low": 120, "high": 150}},
      {"type": "repeat", "count": 2, "steps": [
        {"type": "interval", "duration": {"type": "time", "value": "20min"},
         "target": {"type": "power_range", "low": 185, "high": 195}},
        {"type": "recovery", "duration": {"type": "time", "value": "5min"},
         "target": {"type": "power_range", "low": 100, "high": 120}}]}]}
  }'
```

À partir d'un modèle :

```bash
curl -X POST http://localhost:3001/api/v1/planned \
  -H "Authorization: Bearer $TOKEN" \
  -H 'content-type: application/json' \
  -d '{"date": "2026-10-14", "workout_id": 4, "name": "Seuil (version courte)"}'
```

Erreurs : `400` (date ou sport invalide, ni `workout_id` ni `workout` ou les deux, séance invalide) et `404` (`workout_id` inconnu).

### Format d'une séance (`workout`)

```json
{
  "name": "Nom de la séance",
  "steps": [
    {"type": "warmup|interval|recovery|cooldown",
     "duration": {"type": "time", "value": "15min"},
     "target": {"type": "power_range", "low": 120, "high": 150},
     "terrain": "plat|vallonné|montée",
     "note": "optionnel"},
    {"type": "repeat", "count": 3, "steps": [ "... étapes, y compris d'autres repeat ..." ]}
  ]
}
```

- `duration.value` : `15min`, `90s`, `1h`, `1h30`, `1:30` (m:ss) ou un nombre de minutes.
- `target` (facultatif), au choix :
  - `{"type": "power_range", "low": 185, "high": 195}` — puissance en watts ;
  - `{"type": "power_zone", "zone": 4}` — zone de puissance 1 à 7 (Coggan étendu, en % du FTP : 1 < 55 %, 2 < 75 %, 3 < 90 %, 4 < 105 %, 5 < 120 %, 6 < 150 %, 7 au-delà) ;
  - `{"type": "hr_range", "low": 150, "high": 160}` — fréquence cardiaque en bpm ;
  - `{"type": "hr_zone", "zone": 2}` — zone de FC 1 à 5 (Karvonen, d'après FC repos/max : 1 < 60 % de la réserve, 2 < 70 %, 3 < 80 %, 4 < 90 %, 5 au-delà) ;
  - `{"type": "pace_range", "low": "4:25", "high": "4:35"}` — allure de course en secondes par km (nombre) ou texte `m:ss` ; `low` est le côté **rapide** (le plus petit nombre de secondes), `low` ≤ `high`. Réponses : toujours en secondes (`265.0`).

  `low` ≤ `high` pour les plages. Les cibles FC et allure n'ont pas de watts : leur hauteur sur le graphique et les estimations de TSS utilisent une intensité équivalente — puissance typique de la zone FC (≈ 50 / 65 / 82 / 97 / 110 % du FTP pour les zones 1 à 5), ou allure seuil / allure de l'étape pour une cible d'allure (0,75 si l'allure seuil n'est pas réglée).
- `terrain` (défaut `plat`) et `note` sont facultatifs.
- Il faut un nom non vide, au moins une étape, des durées valides et `count` ≥ 1 avec au moins une étape dans chaque `repeat`.

## DELETE /api/v1/planned/{id}

Retire une séance du calendrier. `204` sans corps en cas de succès, `404` si elle n'existe pas.
