# Fonctionnalités

Comportement détaillé de CyclAI. Pour la vue d'ensemble et l'installation, voir le [README](../README.md) ; pour le fonctionnement interne, [ARCHITECTURE.md](ARCHITECTURE.md).

- [Sources d'activités](#sources-dactivités)
- [Métriques d'entraînement](#métriques-dentraînement)
- [Segments de référence](#segments-de-référence)
- [Bilan d'activité](#bilan-dactivité)
- [Digest hebdomadaire](#digest-hebdomadaire)
- [Mode sans clé API : rapport IA](#mode-sans-clé-api--rapport-ia)
- [Concepteur de séances](#concepteur-de-séances)
- [Séances planifiées et calendrier](#séances-planifiées-et-calendrier)
- [Page d'une activité](#page-dune-activité)
- [Statistiques](#statistiques)
- [Itinéraire GPX](#itinéraire-gpx)
- [Paramètres](#paramètres)

## Sources d'activités

CyclAI fonctionne **seul** : l'import de fichiers est toujours disponible. Strava et Garmin Connect sont des **plugins**, désactivés par défaut et activés à la main dans `/settings` > Sources d'activités (un compte Strava déjà configuré reste activé).

| Source | Comment | Notes |
|---|---|---|
| **Import FIT / GPX** | Bouton « Importer GPX/FIT » du dashboard | Toujours disponible, sans compte. FIT : puissance, FC, cadence, laps. GPX : position, altitude, temps seulement. Source `file`, **id négatif** (pas de lien Strava) |
| **Strava** (plugin) | Identifiants dans `/settings` | Ne garde que `sport_type` = `Ride`, `VirtualRide`, `Run`, `TrailRun` (le champ historique `type` confond route et gravel, il n'est pas utilisé) |
| **Garmin Connect** (plugin) | Clé API personnelle dans `/settings` (Garmin Connect > Paramètres > API personnelle, valable 30 à 365 jours) | Vélo de route, home-trainer et course à pied ; chaque activité est récupérée avec son `.fit` d'origine. Source `garmin`, **id négatif** |

Le bouton **Synchroniser** (ou `cyclai sync`) interroge **toutes les sources activées et configurées**, dans l'ordre de priorité réglé dans `/settings`. Une source qui échoue n'empêche pas les autres ; l'erreur s'affiche à la fin.

**Superposition.** Une activité qui chevauche une activité déjà enregistrée (plus de N secondes en commun, N = « chevauchement toléré », 10 s par défaut, réglable) est **ignorée** : la source passée en premier l'emporte. Une activité ignorée est mémorisée (`ignored_activities`) pour ne pas être retéléchargée. Le fichier importé à la main n'est jamais ignoré, mais il bloque ensuite les activités des plugins qui le recouvrent. Changer l'ordre ne modifie pas ce qui est déjà en base : seules les prochaines synchronisations en tiennent compte, et pour réappliquer un ordre à tout l'historique il faut le réimporter.

Pour chaque activité Strava nouvelle, le sync récupère le résumé, la description, les streams (`latlng, distance, watts, heartrate, altitude, cadence, time`) et les laps (≈ 3 appels). Une activité déjà stockée est ignorée. Après l'import, les métriques et les segments sont recalculés sur **tout** l'historique.

`sync --full` ignore la date du dernier sync de chaque source (et la liste des activités ignorées) et parcourt tout l'historique. Limite de débit Strava (100 requêtes / 15 min) : relancer reprend avec ce qui manque.

`/route` : charge un GPX d'itinéraire (trace ou `<rte>`), affiche le tracé sur la carte et le profil d'altitude **coloré par pente** (survol synchronisé avec la carte, courbe de pente optionnelle), avec distance, D+ / D−, altitude min / max et pentes maximales. L'altitude est lissée sur ~120 m et la pente estimée sur ~100 m, sinon le bruit GPS la fait osciller. Rien n'est enregistré.

## Paramètres

`/settings` regroupe tout ce qui était auparavant dans `.env`, plus l'apparence :

- **Apparence** : couleur d'accent de l'interface (9 teintes douces prédéfinies, ou une couleur personnalisée) et mode **clair / sombre / automatique** (suit le système). Les couleurs de données (zones, puissance, FC…) restent fixes. Le mode sombre s'applique aussi aux graphiques, au calendrier et aux cartes.
- **Sources d'activités** : activation et ordre de priorité des plugins, chevauchement toléré, identifiants Strava et clé API Garmin.
- **Intelligence artificielle** : clé Anthropic, profil cycliste, prompts.
- **Profil physiologique** : FTP, FC max, FC repos, FC seuil, coefficient hrTSS, allure seuil. Enregistrer **recalcule tout l'historique**.
- **Profil cycliste** : texte libre réinjecté dans chaque prompt.
- **Prompts système** du bilan et du digest, éditables.
