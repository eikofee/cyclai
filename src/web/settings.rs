use super::{escape_html, AppState};
use axum::extract::State;
use axum::response::{Html, Redirect};
use serde::Deserialize;

/// Muted, modern accent presets (name, #rrggbb). The native color input stays
/// available for anything else.
const THEME_PRESETS: [(&str, &str); 9] = [
    ("Bleu", "#2563eb"),
    ("Indigo", "#5865c4"),
    ("Violet doux", "#7c6bc4"),
    ("Sauge", "#4f8a78"),
    ("Vert", "#3f8f5f"),
    ("Ocre", "#b9852f"),
    ("Terracotta", "#c0674b"),
    ("Rose poudré", "#be5f80"),
    ("Gris bleu", "#64748b"),
];

fn page_shell(body: &str) -> String {
    let header = super::layout::page_header("Paramètres", "Apparence, entraînement, sources d'activités et intelligence artificielle.", "");
    let content = format!("{header}{body}");
    super::layout::Page::new("Paramètres", super::layout::Nav::Settings, &content)
        .class("narrow")
        .css("textarea.settings-textarea { font-family: ui-monospace, SFMono-Regular, Menlo, monospace; font-size: 0.82rem; min-height: 200px; width: 100%; }
input.color-input { width: 36px; height: 36px; padding: 2px; margin: 0; cursor: pointer; border-radius: 50%; }
.swatches { display: flex; flex-wrap: wrap; align-items: center; gap: 0.55rem; margin-bottom: 0.9rem; }
.swatch { width: 32px; height: 32px; min-width: 0; padding: 0; margin: 0; border-radius: 50%; border: 2px solid var(--card-bg); box-shadow: 0 0 0 1px var(--border-strong); }
.swatch:hover { transform: scale(1.1); border-color: var(--card-bg); }
.swatch.selected { box-shadow: 0 0 0 2px var(--text); }
.swatch-custom { display: inline-flex; align-items: center; gap: 0.4rem; margin-left: 0.5rem; }
.swatch-custom .hint { margin: 0; }
.settings-nav { display: flex; flex-wrap: wrap; gap: 0.4rem; margin: 0 0 1.4rem; position: sticky; top: 0; z-index: 5; padding: 0.5rem 0; background: var(--bg, var(--card-bg)); }
.settings-nav a { padding: 0.3rem 0.8rem; border-radius: 999px; border: 1px solid var(--border-strong); font-size: 0.85rem; text-decoration: none; color: var(--text); }
.settings-nav a:hover { border-color: var(--text); }
.settings-group { margin: 0 0 2.2rem; scroll-margin-top: 3.5rem; }
.settings-group > .group-title { font-size: 0.78rem; text-transform: uppercase; letter-spacing: 0.08em; color: var(--text-muted); margin: 0 0 0.6rem 0.2rem; }
.settings-group > section + section { margin-top: 1rem; }
.source-row { display: flex; align-items: center; gap: 0.6rem; padding: 0.5rem 0; border-bottom: 1px solid var(--border); flex-wrap: wrap; }
.source-row:last-of-type { border-bottom: 0; }
.source-row .grow { flex: 1; min-width: 10rem; }
.source-row label { display: inline-flex; align-items: center; gap: 0.4rem; margin: 0; }
.source-row button { padding: 0.15rem 0.55rem; margin: 0; }
.badge { font-size: 0.72rem; padding: 0.1rem 0.5rem; border-radius: 999px; border: 1px solid var(--border-strong); color: var(--text-muted); }")
        .render()
}

async fn render_page(pool: &sqlx::SqlitePool, error: Option<&str>) -> Html<String> {
    let settings = match crate::settings::fetch(pool).await {
        Ok(s) => s,
        Err(err) => {
            tracing::error!(?err, "failed to load settings");
            return Html(page_shell("<p>Impossible de charger les paramètres.</p>"));
        }
    };
    let strava_refresh_token: Option<String> =
        sqlx::query_scalar::<_, Option<String>>("SELECT strava_refresh_token FROM sync_state WHERE id = 1")
            .fetch_optional(pool)
            .await
            .ok()
            .flatten()
            .flatten();

    let rules = crate::rules::Rules::load(pool).await.unwrap_or_default();
    let (ftp_watts, hr_max, hr_rest, lthr, hr_tss_coef, run_pace_s): (i64, i64, i64, i64, f64, i64) = sqlx::query_as(
        "SELECT ftp_watts, hr_max, hr_rest, lthr, hr_tss_coef, run_threshold_pace_s FROM athlete_config WHERE id = 1",
    )
    .fetch_one(pool)
    .await
    .unwrap_or((0, 0, 0, 0, 1.0, 0));
    let run_pace = if run_pace_s > 0 { super::format::format_pace_secs(run_pace_s as f64) } else { String::new() };

    let error_html = error
        .map(|e| format!(r#"<p class="form-error">{}</p>"#, escape_html(e)))
        .unwrap_or_default();

    let slots = crate::availability::defaults(pool).await.unwrap_or_default();
    let availability_rows: String = slots
        .iter()
        .enumerate()
        .map(|(i, s)| {
            format!(
                r#"  <div class="avail-row"><strong>{day}</strong>
    <input type="number" name="min_{i}" value="{min}" min="0" max="720" aria-label="Minutes {day}"> min
    <select name="tod_{i}">{tod}</select>
    <select name="ctx_{i}">{ctx}</select></div>
"#,
                day = crate::availability::WEEKDAYS[i],
                min = s.available_min,
                tod = select_options(&crate::availability::TIMES_OF_DAY, &s.time_of_day),
                ctx = select_options(&crate::availability::CONTEXTS, &s.context),
            )
        })
        .collect();
    let today = chrono::Utc::now().date_naive();
    let exceptions = crate::availability::exceptions_from(pool, today).await.unwrap_or_default();
    let availability_exceptions = if exceptions.is_empty() {
        r#"<p class="hint">Aucune exception à venir.</p>"#.to_string()
    } else {
        exceptions
            .iter()
            .map(|(date, s, note)| {
                format!(
                    r#"<form method="post" action="/settings/availability/delete" class="avail-row">{date} : {min} min, {tod}, {ctx}{note} <input type="hidden" name="date" value="{date}"><button type="submit" class="secondary">Supprimer</button></form>"#,
                    date = escape_html(date),
                    min = s.available_min,
                    tod = escape_html(&s.time_of_day),
                    ctx = escape_html(&s.context),
                    note = note.as_deref().map(|n| format!(" — {}", escape_html(n))).unwrap_or_default(),
                )
            })
            .collect()
    };

    let source_cfg = crate::sources::config(pool).await;
    let mut source_rows = String::new();
    for (i, id) in source_cfg.order.iter().enumerate() {
        let configured = crate::sources::is_configured(pool, id).await;
        let enabled = source_cfg.enabled.contains(id);
        let badge = if configured { "identifiants renseignés" } else { "à configurer" };
        let up = if i > 0 { format!(r#"<button type="submit" name="move" value="{id}:up" class="secondary" title="Monter" aria-label="Monter">↑</button>"#) } else { String::new() };
        let down = if i + 1 < source_cfg.order.len() {
            format!(r#"<button type="submit" name="move" value="{id}:down" class="secondary" title="Descendre" aria-label="Descendre">↓</button>"#)
        } else {
            String::new()
        };
        source_rows.push_str(&format!(
            r#"<div class="source-row"><span><strong>{rank}.</strong></span>
  <label class="grow"><input type="checkbox" name="enabled" value="{id}"{checked}> {label} <span class="badge">{badge}</span></label>
  <input type="hidden" name="order" value="{id}">{up}{down}</div>
"#,
            rank = i + 2,
            label = crate::sources::label(id),
            checked = if enabled { " checked" } else { "" },
        ));
    }

    let body = format!(
        r##"{error_html}
<nav class="settings-nav" aria-label="Catégories">
  <a href="#general">Général</a><a href="#entrainement">Entraînement</a><a href="#sources">Sources d'activités</a><a href="#ia">Intelligence artificielle</a>
</nav>

<div class="settings-group" id="general">
<div class="group-title" role="heading" aria-level="2">Général</div>
<section>
<h2>Apparence</h2>
<form method="post" action="/settings/appearance">
  <label>Couleur du thème</label>
  <div class="swatches">{swatches}<span class="swatch-custom"><input type="color" name="theme_color" value="{theme_color}" class="color-input" title="Couleur personnalisée"><span class="hint">personnalisée</span></span></div>
  <label>Mode
  <select name="theme_mode">{theme_modes}</select>
  </label>
  <script>
document.querySelectorAll('.swatch').forEach(function(b) {{
  b.addEventListener('click', function() {{
    document.querySelector('input[name=theme_color]').value = b.dataset.color;
    document.querySelectorAll('.swatch').forEach(function(x) {{ x.classList.toggle('selected', x === b); }});
  }});
}});
</script>
  <p class="hint">La couleur d'accent colore la navigation, les boutons et les liens ; les couleurs des zones et des courbes (puissance, FC…) restent fixes pour rester lisibles d'un écran à l'autre.</p>
  <button type="submit">Enregistrer</button>
  <button type="submit" name="reset" value="1" class="secondary">Réinitialiser</button>
</form>
</section>

</div>

<div class="settings-group" id="entrainement">
<div class="group-title" role="heading" aria-level="2">Entraînement</div>
<section>
<h2>Profil physiologique</h2>
<p class="hint">FTP recalcule rétroactivement TSS/IF/zones de tout l'historique une fois enregistrée. Les FC (max, repos, seuil) recalculent le hrTSS des courses à pied.</p>
<form method="post" action="/settings/athlete">
  <label>{ftp_label} (W)
  <input type="number" name="ftp_watts" value="{ftp_watts}" min="1">
  </label>
  <label>FC max (bpm)
  <input type="number" name="hr_max" value="{hr_max}" min="1">
  </label>
  <label>FC repos (bpm)
  <input type="number" name="hr_rest" value="{hr_rest}" min="1">
  </label>
  <label>FC seuil / LTHR (bpm, 0 = automatique)
  <input type="number" name="lthr" value="{lthr}" min="0">
  </label>
  <label>Coefficient hrTSS dans la charge totale
  <input type="number" name="hr_tss_coef" value="{hr_tss_coef}" min="0" max="10" step="0.05">
  </label>
  <p class="hint">Multiplie le hrTSS des courses quand il s'additionne au TSS puissance du vélo dans CTL/ATL/TSB et les totaux (1 = compte comme un TSS vélo, 0,8 = 20 % de moins, 0 = les courses ne comptent pas). Le hrTSS affiché sur chaque course reste le score brut.</p>
  <label>Allure seuil en course à pied (min/km, vide = non réglée)
  <input type="text" name="run_pace" value="{run_pace}" placeholder="4:30" pattern="[0-9]+:[0-5][0-9]">
  </label>
  <p class="hint">Référence des cibles d'allure du concepteur de séances : sert à placer une étape d'allure sur l'échelle d'intensité (graphique, TSS et distance estimés).</p>
  <p class="hint">Sert de référence au hrTSS (une heure à la FC seuil = 100). Automatique : FC repos + 85 % de la réserve cardiaque.</p>
  <button type="submit">Enregistrer</button>
</form>
</section>

<section>
<h2>Règles d'entraînement</h2>
<p class="hint">Appliquées par le serveur aux séances proposées et aux simulations de charge (l'agent ne peut pas les contourner). Un jour est « intense » si son IF atteint le seuil ou si son TSS atteint le TSS minimum.</p>
<form method="post" action="/settings/rules">
  <label>Gain de CTL maximal sur 7 jours
  <input type="number" name="max_ctl_ramp_per_week" value="{max_ctl_ramp}" min="0" step="0.5">
  </label>
  <label>Plancher de TSB
  <input type="number" name="min_tsb" value="{min_tsb}" min="-100" max="0" step="1">
  </label>
  <label>Jour intense : IF à partir de
  <input type="number" name="intense_if" value="{intense_if}" min="0.5" max="1.5" step="0.01">
  </label>
  <label>Jour intense : TSS du jour à partir de
  <input type="number" name="intense_tss" value="{intense_tss}" min="1" step="1">
  </label>
  <label>Jours intenses consécutifs maximum
  <input type="number" name="max_consecutive_intense" value="{max_consec}" min="1" max="7">
  </label>
  <label>Jours de repos minimum par fenêtre de 7 jours
  <input type="number" name="min_rest_days_per_week" value="{min_rest}" min="0" max="7">
  </label>
  <button type="submit">Enregistrer</button>
</form>
</section>

<section>
<h2>Disponibilités</h2>
<p class="hint">Temps disponible pour rouler, par jour de la semaine. L'agent s'en sert pour dimensionner les séances ; il peut poser des exceptions (vacances, soirées longues), jamais modifier ces règles.</p>
<form method="post" action="/settings/availability">
{availability_rows}
  <button type="submit">Enregistrer</button>
</form>
<h3>Exceptions à venir</h3>
{availability_exceptions}
<form method="post" action="/settings/availability/exception">
  <label>Date
  <input type="date" name="date" required>
  </label>
  <label>Minutes disponibles
  <input type="number" name="available_min" value="120" min="0" max="720" required>
  </label>
  <label>Moment
  <select name="time_of_day">{tod_options}</select>
  </label>
  <label>Contexte
  <select name="context">{ctx_options}</select>
  </label>
  <label>Note
  <input type="text" name="note" maxlength="300">
  </label>
  <button type="submit">Ajouter ou remplacer l'exception</button>
</form>
</section>

</div>

<div class="settings-group" id="sources">
<div class="group-title" role="heading" aria-level="2">Sources d'activités</div>
<section>
<h2>Priorité et activation</h2>
<p class="hint">CyclAI fonctionne seul : l'import de fichiers .fit/.gpx (bouton du tableau de bord) est toujours disponible et passe en premier. Strava et Garmin sont des plugins, désactivés tant que tu ne les actives pas. « Synchroniser » interroge les sources activées dans l'ordre ci-dessous ; une activité qui chevauche une activité déjà enregistrée est ignorée, donc la source la mieux classée l'emporte.</p>
<form method="post" action="/settings/sources">
  <div class="source-row"><span><strong>1.</strong></span><span class="grow">Fichier .fit/.gpx <span class="badge">toujours actif</span></span></div>
{source_rows}
  <label>Chevauchement toléré entre deux activités (secondes)
  <input type="number" name="overlap_tolerance_s" value="{overlap_tolerance_s}" min="0" max="3600" step="1">
  </label>
  <p class="hint">Deux activités ne sont considérées comme superposées que si elles partagent plus de ce nombre de secondes (une sortie qui enchaîne directement sur la suivante ne compte pas).</p>
  <button type="submit">Enregistrer</button>
  <p class="hint">Le nouvel ordre ne joue que pour les prochaines synchronisations : les activités déjà enregistrées ne changent pas. Pour réappliquer un ordre à tout l'historique, il faut le réimporter.</p>
</form>
</section>
<section>
<h2>Strava</h2>
<form method="post" action="/settings/keys">
  <label>Client ID
  <input type="text" name="strava_client_id" value="{strava_client_id}">
  </label>
  <label>Client Secret
  <input type="password" autocomplete="off" name="strava_client_secret" value="{strava_client_secret}">
  </label>
  <label>Refresh Token
  <input type="password" autocomplete="off" name="strava_refresh_token" value="{strava_refresh_token}">
  </label>
  <p class="hint">Renouvelé automatiquement par Strava à chaque synchronisation — à renseigner manuellement seulement pour une première configuration ou après révocation.</p>
  <button type="submit">Enregistrer</button>
</form>
</section>

<section>
<h2>Garmin Connect</h2>
<form method="post" action="/settings/keys">
  <label>Clé API personnelle
  <input type="password" autocomplete="off" name="garmin_api_key" value="{garmin_api_key}">
  </label>
  <p class="hint">À créer dans Garmin Connect, sous Paramètres → API personnelle (elle expire après 30, 90, 180 ou 365 jours). Seuls le vélo de route, le home-trainer et la course à pied sont importés ; chaque activité est récupérée avec son fichier .fit d'origine.</p>
  <button type="submit">Enregistrer</button>
</form>
</section>
</div>

<div class="settings-group" id="ia">
<div class="group-title" role="heading" aria-level="2">Intelligence artificielle</div>
<section>
<h2>Clé API Anthropic</h2>
<form method="post" action="/settings/keys">
  <label>Clé API
  <input type="password" autocomplete="off" name="anthropic_api_key" value="{anthropic_api_key}">
  </label>
  <p class="hint">Optionnelle : sans clé, les bilans/digests ne sont pas générés automatiquement, mais un « rapport IA » est téléchargeable depuis le dashboard et les pages d'activité pour le coller dans Claude, ChatGPT…</p>
  <button type="submit">Enregistrer</button>
</form>
</section>

<section>
<h2>Profil cycliste</h2>
<p class="hint">Réinjecté dans chaque prompt envoyé au LLM (bilans et digest hebdomadaire) : FTP/zones, objectifs, équipement, contraintes...</p>
<form method="post" action="/settings/profile">
  <textarea class="settings-textarea" name="athlete_profile">{athlete_profile}</textarea>
  <button type="submit">Enregistrer</button>
</form>
</section>

<section>
<h2>Prompt système — bilan d'activité</h2>
<p class="hint">Envoyé au LLM à chaque génération/régénération du bilan d'une sortie.</p>
<form method="post" action="/settings/prompts/bilan">
  <textarea class="settings-textarea" name="system_prompt">{bilan_system_prompt}</textarea>
  <button type="submit">Enregistrer</button>
</form>
</section>

<section>
<h2>Prompt système — digest hebdomadaire</h2>
<p class="hint">Envoyé au LLM à chaque génération du digest hebdomadaire (recommandation de séance).</p>
<form method="post" action="/settings/prompts/digest">
  <textarea class="settings-textarea" name="system_prompt">{digest_system_prompt}</textarea>
  <button type="submit">Enregistrer</button>
</form>
</section>
</div>
"##,
        error_html = error_html,
        theme_color = escape_html(&settings.theme_color),
        swatches = THEME_PRESETS
            .iter()
            .map(|(name, hex)| format!(
                r#"<button type="button" class="swatch{}" data-color="{hex}" style="background:{hex}" title="{name}" aria-label="{name}"></button>"#,
                if hex.eq_ignore_ascii_case(&settings.theme_color) { " selected" } else { "" }
            ))
            .collect::<String>(),
        theme_modes = super::layout::MODES
            .iter()
            .map(|(v, l)| format!(r#"<option value="{v}"{}>{l}</option>"#, if *v == settings.theme_mode { " selected" } else { "" }))
            .collect::<String>(),
        ftp_label = super::annotate_acronyms("FTP"),
        ftp_watts = ftp_watts,
        hr_max = hr_max,
        hr_rest = hr_rest,
        lthr = lthr,
        hr_tss_coef = hr_tss_coef,
        availability_rows = availability_rows,
        availability_exceptions = availability_exceptions,
        tod_options = select_options(&crate::availability::TIMES_OF_DAY, "soir"),
        ctx_options = select_options(&crate::availability::CONTEXTS, "autre"),
        max_ctl_ramp = rules.max_ctl_ramp_per_week,
        min_tsb = rules.min_tsb,
        intense_if = rules.intense_if,
        intense_tss = rules.intense_tss,
        max_consec = rules.max_consecutive_intense,
        min_rest = rules.min_rest_days_per_week,
        run_pace = run_pace,
        source_rows = source_rows,
        overlap_tolerance_s = source_cfg.tolerance_s,
        garmin_api_key = escape_html(&source_cfg.garmin_api_key),
        strava_client_id = escape_html(&settings.strava_client_id),
        strava_client_secret = escape_html(&settings.strava_client_secret),
        strava_refresh_token = escape_html(strava_refresh_token.as_deref().unwrap_or("")),
        anthropic_api_key = escape_html(&settings.anthropic_api_key),
        athlete_profile = escape_html(&settings.athlete_profile),
        bilan_system_prompt = escape_html(&settings.bilan_system_prompt),
        digest_system_prompt = escape_html(&settings.digest_system_prompt),
    );

    Html(page_shell(&body))
}

pub async fn page(State(state): State<AppState>) -> Html<String> {
    render_page(&state.pool, None).await
}

#[derive(Deserialize)]
pub struct KeysForm {
    strava_client_id: Option<String>,
    strava_client_secret: Option<String>,
    strava_refresh_token: Option<String>,
    anthropic_api_key: Option<String>,
    garmin_api_key: Option<String>,
}

pub async fn update_keys(State(state): State<AppState>, axum::extract::Form(form): axum::extract::Form<KeysForm>) -> Redirect {
    // Strava and Anthropic have their own forms: only the fields a form sent are written.
    for (column, value) in [
        ("strava_client_id", &form.strava_client_id),
        ("strava_client_secret", &form.strava_client_secret),
        ("anthropic_api_key", &form.anthropic_api_key),
        ("garmin_api_key", &form.garmin_api_key),
    ] {
        if let Some(v) = value {
            let _ = sqlx::query(&format!("UPDATE settings SET {column} = ? WHERE id = 1")).bind(v.trim()).execute(&state.pool).await;
        }
    }

    // sync_state may not have a row yet on a fresh DB (only ever seeded/
    // written by the boot seed or a completed sync) - upsert rather than
    // assume it exists.
    if let Some(token) = &form.strava_refresh_token {
        let _ = sqlx::query(
            r#"
            INSERT INTO sync_state (id, last_sync_epoch, strava_refresh_token)
            VALUES (1, 0, ?)
            ON CONFLICT (id) DO UPDATE SET strava_refresh_token = excluded.strava_refresh_token
            "#,
        )
        .bind(token.trim())
        .execute(&state.pool)
        .await;
    }

    Redirect::to("/settings")
}

#[derive(Deserialize)]
pub struct AthleteConfigForm {
    ftp_watts: i64,
    hr_max: i64,
    hr_rest: i64,
    #[serde(default)]
    lthr: i64,
    #[serde(default = "default_hr_tss_coef")]
    hr_tss_coef: f64,
    #[serde(default)]
    run_pace: String,
}

fn default_hr_tss_coef() -> f64 {
    1.0
}

/// Updates FTP/HR max/HR rest, then immediately re-runs the deterministic
/// metrics pipeline so TSS/IF/zones retroactively reflect the new FTP across
/// the whole history — matches the sync-time behavior in metrics::run.
pub async fn update_athlete_config(
    State(state): State<AppState>,
    axum::extract::Form(form): axum::extract::Form<AthleteConfigForm>,
) -> Redirect {
    let _ = sqlx::query(
        "UPDATE athlete_config SET ftp_watts = ?, hr_max = ?, hr_rest = ?, lthr = ?, hr_tss_coef = ?, run_threshold_pace_s = ?, updated_at = datetime('now') WHERE id = 1",
    )
    .bind(form.ftp_watts)
    .bind(form.hr_max)
    .bind(form.hr_rest)
    .bind(form.lthr.max(0))
    .bind(if form.hr_tss_coef.is_finite() { form.hr_tss_coef.clamp(0.0, 10.0) } else { 1.0 })
    // Blank or unreadable = not set (0).
    .bind(crate::workout::parse_pace(&form.run_pace).filter(|p| *p > 0.0).map(|p| p.round() as i64).unwrap_or(0))
    .execute(&state.pool)
    .await;

    let _ = crate::metrics::run(&state.pool).await;

    Redirect::to("/settings")
}

#[derive(Deserialize)]
pub struct RulesForm {
    max_ctl_ramp_per_week: f64,
    min_tsb: f64,
    intense_if: f64,
    intense_tss: f64,
    max_consecutive_intense: i64,
    min_rest_days_per_week: i64,
}

pub async fn update_rules(State(state): State<AppState>, axum::extract::Form(f): axum::extract::Form<RulesForm>) -> Redirect {
    let d = crate::rules::Rules::default();
    let finite = |v: f64, lo: f64, hi: f64, default: f64| if v.is_finite() { v.clamp(lo, hi) } else { default };
    let _ = sqlx::query(
        "UPDATE athlete_config SET max_ctl_ramp_per_week = ?, min_tsb = ?, intense_if = ?, intense_tss = ?,          max_consecutive_intense = ?, min_rest_days_per_week = ?, updated_at = datetime('now') WHERE id = 1",
    )
    .bind(finite(f.max_ctl_ramp_per_week, 0.0, 50.0, d.max_ctl_ramp_per_week))
    .bind(finite(f.min_tsb, -100.0, 0.0, d.min_tsb))
    .bind(finite(f.intense_if, 0.5, 1.5, d.intense_if))
    .bind(finite(f.intense_tss, 1.0, 1000.0, d.intense_tss))
    .bind(f.max_consecutive_intense.clamp(1, 7))
    .bind(f.min_rest_days_per_week.clamp(0, 7))
    .execute(&state.pool)
    .await;
    Redirect::to("/settings")
}

fn select_options(values: &[&str], selected: &str) -> String {
    values
        .iter()
        .map(|v| format!(r#"<option value="{v}"{}>{v}</option>"#, if *v == selected { " selected" } else { "" }))
        .collect()
}

/// Weekday defaults: fields `min_0`…`min_6`, `tod_0`…, `ctx_0`… (Monday = 0).
/// Rows that don't validate keep their current value.
pub async fn update_availability(State(state): State<AppState>, axum::extract::Form(fields): axum::extract::Form<Vec<(String, String)>>) -> Redirect {
    let get = |k: String| fields.iter().find(|(n, _)| *n == k).map(|(_, v)| v.as_str());
    let mut slots = crate::availability::defaults(&state.pool).await.unwrap_or_default();
    for (i, slot) in slots.iter_mut().enumerate() {
        let candidate = crate::availability::Slot {
            available_min: get(format!("min_{i}")).and_then(|v| v.trim().parse().ok()).unwrap_or(slot.available_min),
            time_of_day: get(format!("tod_{i}")).unwrap_or(&slot.time_of_day).to_string(),
            context: get(format!("ctx_{i}")).unwrap_or(&slot.context).to_string(),
        };
        if crate::availability::validate(&candidate).is_ok() {
            *slot = candidate;
        }
    }
    let _ = crate::availability::save_defaults(&state.pool, &slots).await;
    Redirect::to("/settings")
}

#[derive(Deserialize)]
pub struct AvailabilityExceptionForm {
    date: String,
    available_min: i64,
    time_of_day: String,
    context: String,
    #[serde(default)]
    note: String,
}

pub async fn add_availability_exception(State(state): State<AppState>, axum::extract::Form(f): axum::extract::Form<AvailabilityExceptionForm>) -> Redirect {
    let slot = crate::availability::Slot { available_min: f.available_min, time_of_day: f.time_of_day, context: f.context };
    if let (Some(date), Ok(())) = (crate::planned::parse_date(&f.date), crate::availability::validate(&slot)) {
        let note: String = f.note.trim().chars().take(300).collect();
        let _ = crate::availability::set_exception(&state.pool, date, &slot, Some(note.as_str()).filter(|n| !n.is_empty())).await;
    }
    Redirect::to("/settings")
}

#[derive(Deserialize)]
pub struct DateForm {
    date: String,
}

pub async fn delete_availability_exception(State(state): State<AppState>, axum::extract::Form(f): axum::extract::Form<DateForm>) -> Redirect {
    let _ = crate::availability::delete_exception(&state.pool, f.date.trim()).await;
    Redirect::to("/settings")
}

#[derive(Deserialize)]
pub struct AppearanceForm {
    #[serde(default)]
    theme_color: String,
    #[serde(default)]
    theme_mode: String,
    #[serde(default)]
    reset: String,
}

pub async fn update_appearance(State(state): State<AppState>, axum::extract::Form(form): axum::extract::Form<AppearanceForm>) -> Redirect {
    let (color, mode) = if !form.reset.is_empty() {
        (super::layout::DEFAULT_ACCENT.to_string(), "auto".to_string())
    } else {
        let color = form.theme_color.trim().to_lowercase();
        (
            if super::layout::valid_color(&color) { color } else { super::layout::DEFAULT_ACCENT.to_string() },
            if super::layout::MODES.iter().any(|(m, _)| *m == form.theme_mode) { form.theme_mode } else { "auto".to_string() },
        )
    };
    let _ = sqlx::query("UPDATE settings SET theme_color = ?, theme_mode = ? WHERE id = 1")
        .bind(&color)
        .bind(&mode)
        .execute(&state.pool)
        .await;
    super::layout::set_theme(&color, &mode);
    Redirect::to("/settings")
}

#[derive(Deserialize)]
pub struct ProfileForm {
    #[serde(default)]
    athlete_profile: String,
}

pub async fn update_profile(State(state): State<AppState>, axum::extract::Form(form): axum::extract::Form<ProfileForm>) -> Redirect {
    let _ = sqlx::query("UPDATE settings SET athlete_profile = ? WHERE id = 1")
        .bind(&form.athlete_profile)
        .execute(&state.pool)
        .await;
    Redirect::to("/settings")
}

#[derive(Deserialize)]
pub struct PromptForm {
    system_prompt: String,
}

pub async fn update_bilan_prompt(State(state): State<AppState>, axum::extract::Form(form): axum::extract::Form<PromptForm>) -> Redirect {
    let _ = sqlx::query("UPDATE settings SET bilan_system_prompt = ? WHERE id = 1")
        .bind(&form.system_prompt)
        .execute(&state.pool)
        .await;
    Redirect::to("/settings")
}

pub async fn update_digest_prompt(State(state): State<AppState>, axum::extract::Form(form): axum::extract::Form<PromptForm>) -> Redirect {
    let _ = sqlx::query("UPDATE settings SET digest_system_prompt = ? WHERE id = 1")
        .bind(&form.system_prompt)
        .execute(&state.pool)
        .await;
    Redirect::to("/settings")
}

/// `POST /settings/sources` — which plugins are enabled, in which priority
/// order, and the tolerated overlap. A `move` button (`<id>:up` / `<id>:down`)
/// also saves the rest of the form, so nothing typed is lost.
pub async fn update_sources(State(state): State<AppState>, axum::extract::Form(fields): axum::extract::Form<Vec<(String, String)>>) -> Redirect {
    let values = |name: &str| fields.iter().filter(|(k, _)| k == name).map(|(_, v)| v.clone()).collect::<Vec<_>>();
    let mut order = crate::sources::normalize_order(&values("order"));
    if let Some(m) = values("move").first()
        && let Some((id, dir)) = m.split_once(':')
        && let Some(i) = order.iter().position(|s| s == id)
    {
        let j = if dir == "up" { i.checked_sub(1) } else { Some(i + 1).filter(|j| *j < order.len()) };
        if let Some(j) = j {
            order.swap(i, j);
        }
    }
    if let Err(err) = crate::sources::save_order_and_enabled(&state.pool, &order, &values("enabled")).await {
        tracing::error!(?err, "failed to save source order");
    }
    if let Some(Ok(seconds)) = values("overlap_tolerance_s").first().map(|v| v.trim().parse::<i64>())
        && let Err(err) = crate::sources::save_tolerance(&state.pool, seconds).await
    {
        tracing::error!(?err, "failed to save overlap tolerance");
    }
    Redirect::to("/settings#sources")
}
