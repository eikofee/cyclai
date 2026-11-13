# Plugins d'activités

CyclAI fonctionne seul : l'import de fichiers `.fit` / `.gpx` est toujours disponible, sans compte ni réseau. Pour récupérer automatiquement ses sorties depuis un service (par exemple Garmin Connect), on branche un **plugin**.

Un plugin est un **petit service HTTP, dans son propre conteneur**. Il détient les identifiants de la source ; CyclAI l'appelle, et jamais l'inverse. Un plugin n'a donc aucun accès à CyclAI, ni à sa base, ni à son API, et CyclAI ne connaît pas les identifiants de la source.

| Plugin | Dépôt | Source |
|---|---|---|
| Garmin Connect | `cyclai-garmin` | API personnelle Garmin, `.fit` d'origine |

## Brancher un plugin

1. Construis l'image du plugin (voir son README) et ajoute son service au `docker-compose.yml`, **sans publier de port** : seul CyclAI, sur le même réseau compose, doit pouvoir l'atteindre.
2. Déclare-le pour CyclAI : `CYCLAI_PLUGINS` liste les plugins sous la forme `id=url`, **séparés par des virgules**, et chaque plugin a sa variable de secret `CYCLAI_PLUGIN_<ID>_SECRET` (l'id en majuscules) :

   ```yaml
   cyclai-ui:
     environment:
       CYCLAI_PLUGINS: "garmin=http://garmin:8080,monplugin=http://monplugin:8080"
       CYCLAI_PLUGIN_GARMIN_SECRET: "${CYCLAI_PLUGIN_GARMIN_SECRET}"
       CYCLAI_PLUGIN_MONPLUGIN_SECRET: "${CYCLAI_PLUGIN_MONPLUGIN_SECRET}"
   ```

   Ces secrets ne sont **pas** les identifiants de la source (la clé API Garmin, par exemple, reste dans le conteneur du plugin) : c'est un mot de passe entre CyclAI et le plugin, qui sert à signer les requêtes. Il est propre à chaque plugin : donne le **même** secret au plugin (`CYCLAI_PLUGIN_SECRET`) et à CyclAI (`CYCLAI_PLUGIN_<ID>_SECRET`, l'id en majuscules). 16 caractères minimum ; une longue phrase aléatoire convient. Le secret n'est jamais affiché dans l'interface.
3. Ouvre **Paramètres > Sources d'activités** : le plugin y apparaît avec son état (prêt, à configurer, injoignable). Coche-le pour l'activer et classe les sources par priorité.
4. **Synchroniser** (ou `cyclai sync`) interroge les plugins activés, dans l'ordre.

Les plugins ne se déclarent que dans l'environnement, jamais dans l'interface web : une URL saisie dans un formulaire pourrait faire appeler n'importe quelle adresse du réseau.

## Priorité et chevauchements

Les plugins activés passent dans l'ordre de la page Paramètres (le fichier importé à la main est toujours le premier). Une activité qui **partage plus de N secondes** avec une activité déjà enregistrée est ignorée (N = « chevauchement toléré », 10 s par défaut) : c'est donc la source passée en premier qui l'emporte. Une activité écartée est mémorisée pour ne pas être retéléchargée ; `sync --full` la réexamine.

Changer l'ordre ne modifie pas l'historique déjà enregistré : il ne joue que pour les prochaines synchronisations.

## Le protocole

Version 1, HTTP/1.1, JSON. Le crate [`plugin-sdk`](../plugin-sdk) fournit les types, la signature, un client (côté CyclAI) et un serveur (côté plugin) : un plugin en Rust n'a qu'à implémenter le trait `Plugin` et appeler `serve`. Rien n'empêche d'en écrire un dans un autre langage.

| Requête | Réponse |
|---|---|
| `GET /manifest` | `{protocol, id, label, version, sports, configured, message?, activity_url?, activity_url_label?}` |
| `GET /activities?after=<epoch>` | `{"activities": [{external_id, title, started_at, start_epoch, duration_s?, sport_type}]}` des activités commencées après cette date (0 = tout l'historique) |
| `GET /activities/{external_id}` | une activité en JSON (`application/json`), ou le `.fit` d'origine (`application/vnd.ant.fit`) ; **404** s'il n'y a rien à importer |

- `id` : 1 à 32 minuscules ou chiffres, identique à celui déclaré dans `CYCLAI_PLUGINS`. Il est enregistré avec chaque activité (`source`).
- `sport_type` : `Ride`, `GravelRide`, `VirtualRide`, `Run` ou `TrailRun` ; le reste est écarté par CyclAI.
- `duration_s` dans la liste est facultatif mais utile : s'il est connu, une activité qui chevauche une autre est écartée **avant** d'être téléchargée.
- `activity_url` est un modèle de lien vers l'activité sur le site de la source (`https://…/{id}`, en HTTPS, avec `{id}`) ; il alimente le lien « voir sur … » des pages d'activité.
- L'activité JSON porte `started_at` (RFC 3339), `sport_type`, `duration_s`, `distance_m`, `elevation_gain_m`, la puissance (moyenne et normalisée, vélo seulement), la FC, `title`, `description?`, des `streams` (`{"time": {"data": [...]}, "watts": …, "heartrate": …, "latlng": …, "altitude": …, "distance": …, "cadence": …}`) et des `laps`. Renvoyer le `.fit` d'origine est plus simple : CyclAI sait l'analyser.
- Erreurs : un statut HTTP et `{"error": "..."}`. **429** (avec `Retry-After`) pour une limite de débit : CyclAI s'arrête et la synchronisation suivante reprend où elle en était. **502** pour un échec de la source.

## Signature des requêtes

Chaque requête de CyclAI porte deux en-têtes :

- `X-Cyclai-Timestamp` : secondes Unix ;
- `X-Cyclai-Signature` : HMAC-SHA256 en hexadécimal, avec le secret partagé, de `timestamp \n MÉTHODE \n chemin?requête \n sha256(corps)`.

Le plugin refuse (401, sans détail) une requête non signée, signée avec un autre secret, modifiée, ou dont l'horodatage s'écarte de plus de 60 secondes (ce qui borne un rejeu). La comparaison est à temps constant. Les secrets étant propres à chaque plugin, un plugin compromis ne peut pas se faire passer pour CyclAI auprès d'un autre.

Sur le réseau interne d'un compose, cette signature suffit. Si le trafic devait traverser un réseau non fiable, place un TLS devant le plugin (l'URL déclarée peut être en `https://`).

## Ce que CyclAI vérifie

Ce qu'un plugin renvoie est traité comme **non fiable** : réponses de taille limitée (liste 16 Mo, activité 64 Mo), délais d'attente, sport reconnu, durée plausible (30 jours au plus), nombres finis, textes nettoyés et tronqués, puissance ignorée hors vélo, liens limités à HTTPS. Les redirections ne sont pas suivies. Tout passe ensuite par des requêtes SQL paramétrées.

## Écrire un plugin

En Rust, avec le SDK (`features = ["server"]`) :

```rust
struct MaSource;

#[async_trait]
impl Plugin for MaSource {
    async fn manifest(&self) -> Manifest { /* id, label, configured… */ }
    async fn list(&self, after_epoch: i64) -> Result<Vec<ListedActivity>, PluginError> { /* … */ }
    async fn fetch(&self, external_id: &str) -> Result<Fetched, PluginError> { /* Activity ou Fit */ }
}

serve(Arc::new(MaSource), "0.0.0.0:8080".parse()?, secret).await
```

Voir `cyclai-garmin` : il renvoie le `.fit` d'origine de chaque activité.
