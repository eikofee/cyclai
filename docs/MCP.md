# Serveur MCP et connexion de Claude (OAuth)

CyclAI expose un serveur **MCP** (Model Context Protocol, transport « streamable HTTP ») sur le processus API (`cyclai serve-api`, port 3001, publié par Tailscale Funnel sur 8443). Claude web / mobile s'y connecte comme « connecteur personnalisé » et s'authentifie en **OAuth 2.1**.

Principe : *le moteur calcule, l'agent choisit et explique.* Les règles dures (rampe de CTL, plancher de TSB, jours intenses consécutifs, jour de repos) sont appliquées côté serveur ; une proposition est stockée `proposed` et c'est l'utilisateur qui la confirme dans le dashboard. **Aucun outil ne supprime quoi que ce soit.**

## Mise en route

1. Définir l'URL publique dans `.env` (annoncée dans les métadonnées OAuth) :
   ```
   PUBLIC_BASE_URL=https://<machine>.<tailnet>.ts.net:8443
   ```
2. Définir le mot de passe de la page de consentement (12 caractères minimum) :
   ```bash
   docker compose exec cyclai-api cyclai oauth set-password
   ```
3. Dans Claude : *Paramètres → Connecteurs → Ajouter un connecteur personnalisé*, URL `https://<machine>.<tailnet>.ts.net:8443/mcp`.
4. Claude s'enregistre seul (*dynamic client registration*), ouvre la page d'autorisation de CyclAI ; saisir le mot de passe et cliquer **Autoriser**. Il faut le ressaisir seulement si l'accès a expiré ou a été révoqué (voir ci-dessous), pas à chaque appel.

## Gestion des accès

```bash
cyclai oauth list                  # connecteurs enregistrés, dernière utilisation
cyclai oauth revoke <client_id>    # ou `all` : les jetons cessent de fonctionner immédiatement
cyclai oauth set-refresh-days 90   # inactivité maximale avant ré-autorisation (défaut 90 j)
```

- Jeton d'accès : 1 h. Jeton de rafraîchissement : **rotatif** (chaque usage en émet un nouveau et fait glisser la fenêtre d'inactivité de 90 jours). Rejouer un ancien jeton révoque tous les jetons du connecteur.
- Codes et jetons ne sont stockés que sous forme de SHA-256 ; le mot de passe en argon2.
- 5 mots de passe erronés verrouillent la page de consentement 15 minutes.
- Chaque appel d'outil est journalisé dans `mcp_audit` (client, outil, arguments, résultat).
- Limites : 120 appels/min par connecteur, dont 20/min pour les outils d'écriture (`rate_limited` avec délai d'attente).

## Portées (scopes)

| Scope | Donne accès à |
|---|---|
| `cyclai:read` | tout ce qui lit ou simule |
| `cyclai:propose` | `propose_session`, `set_availability`, `mark_session_skipped`, `log_checkin`, `set_coach_note` |

`tools/list` ne montre que les outils autorisés par les scopes du jeton.

## Outils

| Outil | Scope | Rôle |
|---|---|---|
| `get_status` | read | Synthèse 7 j, charge 14 j, calendrier, note du coach |
| `get_athlete_profile` | read | FTP, zones, forme, objectifs |
| `get_rules` | read | Règles dures appliquées par le serveur |
| `get_load_history` | read | Série TSS / CTL / ATL / TSB |
| `list_activities`, `get_activity` | read | Sorties réalisées (`indoor`, `power_source`, `has_hr`…) |
| `search_workouts`, `get_workout` | read | Bibliothèque de séances (Zwift…), IF/TSS/zones calculés au FTP courant |
| `get_plan` | read | Séances du calendrier et leur statut |
| `get_availability` | read | Temps disponible jour par jour |
| `get_ride_envelope` | read | Scénarios de sortie possibles un jour donné, faisabilité vs règles |
| `project_load` | read | Simulation de charge et violations (n'écrit rien) |
| `propose_session` | propose | Propose une séance (`workout_id` ou `free_ride_budget`) ; statut `proposed` |
| `set_availability` | propose | Exception de disponibilité (jour ou plage) |
| `mark_session_skipped` | propose | Marque une séance à venir comme passée |
| `log_checkin` | propose | Ressenti du jour |
| `set_coach_note` | propose | Message affiché en haut du dashboard |

Chaque outil appelle en interne la route `/api/v1` correspondante : mêmes validations, mêmes règles, mêmes limites que l'API REST (voir `API.md`).

## Erreurs

Un échec d'outil renvoie `isError: true` et un contenu structuré :

```json
{ "code": "rule_violation", "message": "…", "hint": "…", "details": { … } }
```

Codes : `invalid_argument`, `not_found`, `conflict`, `rule_violation`, `forbidden`, `insufficient_scope`, `rate_limited`, `internal_error`.

## Endpoints

| Chemin | Rôle |
|---|---|
| `POST /mcp` | JSON-RPC 2.0 (`initialize`, `ping`, `tools/list`, `tools/call`) |
| `/.well-known/oauth-protected-resource` | Métadonnées de la ressource (RFC 9728) |
| `/.well-known/oauth-authorization-server` | Métadonnées du serveur d'autorisation (RFC 8414) |
| `POST /oauth/register` | Enregistrement dynamique de client (RFC 7591), clients publics |
| `GET/POST /oauth/authorize` | Page de consentement (PKCE S256 obligatoire) |
| `POST /oauth/token` | `authorization_code` et `refresh_token` |

Pour les scripts et les tests, `/mcp` accepte aussi un JWT RS256 d'une clé `cyclai api-key` (clé `write` = les deux scopes, clé `read` = lecture seule).
