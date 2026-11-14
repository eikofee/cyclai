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
.avail-row { display: grid; grid-template-columns: 6.5rem auto auto minmax(0, 1fr); gap: 0.4rem 1.2rem; align-items: center; padding: 0.55rem 0; border-bottom: 1px solid var(--border); }
.avail-row:last-of-type { border-bottom: 0; }
.avail-day { font-weight: 600; }
.avail-ctl { display: flex; flex-wrap: wrap; align-items: center; gap: 0.5rem 1.2rem; }
.avail-row .avail-ctl { display: contents; }
.avail-dur { display: inline-flex; align-items: center; gap: 0.35rem; color: var(--text-muted); font-size: 0.85rem; }
.avail-dur input { width: 4.2rem; margin: 0; }
label.avail-free { display: inline-flex; align-items: center; gap: 0.45rem; margin: 0; white-space: nowrap; color: var(--text); }
.avail-tod select { margin: 0; }
.avail-exc { display: flex; justify-content: space-between; align-items: center; gap: 0.8rem; padding: 0.4rem 0; border-bottom: 1px solid var(--border); font-size: 0.9rem; }
.avail-exc button { margin: 0; padding: 0.15rem 0.6rem; }
@media (max-width: 640px) { .avail-row { grid-template-columns: 1fr 1fr; } .avail-day { grid-column: 1 / -1; } }
.field { display: grid; grid-template-columns: minmax(0, 17rem) minmax(0, 1fr); gap: 0.2rem 1.2rem; align-items: start; margin: 0 0 0.9rem; }
.field > label { margin: 0; padding-top: 0.55rem; }
.field .control input, .field .control select { margin-top: 0; }
.field .control input[type=number], .field .control input[type=date], .field .control input.short { max-width: 12rem; }
.field .control .hint { margin: 0.35rem 0 0; font-size: 0.8rem; }
.field .swatches { margin: 0.2rem 0 0; }
@media (max-width: 640px) { .field { grid-template-columns: minmax(0, 1fr); } .field > label { padding-top: 0; } }
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
    let rules = crate::rules::Rules::load(pool).await.unwrap_or_default();
    let (ftp_watts, hr_max, hr_rest, lthr, hr_tss_coef, run_pace_s): (i64, i64, i64, i64, f64, i64) = sqlx::query_as(
        "SELECT ftp_watts, hr_max, hr_rest, lthr, hr_tss_coef, run_threshold_pace_s FROM athlete_config WHERE id = 1",
    )
    .fetch_one(pool)
    .await
    .unwrap_or((0, 0, 0, 0, 1.0, 0));
    let weight_kg: f64 = sqlx::query_scalar("SELECT weight_kg FROM athlete_config WHERE id = 1").fetch_one(pool).await.unwrap_or(0.0);
    let weight = if weight_kg > 0.0 { format!("{weight_kg}") } else { String::new() };
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
                r#"  <div class="avail-row"><strong class="avail-day">{day}</strong>{ctl}</div>
"#,
                day = crate::availability::WEEKDAYS[i],
                ctl = avail_inputs(&i.to_string(), s.available_min, &s.time_of_day),
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
                let slot = match s.time_of_day.as_str() {
                    "journee" => "journée libre",
                    "matin" => "matin seulement",
                    _ => "soir seulement",
                };
                format!(
                    r#"<form method="post" action="/settings/availability/delete" class="avail-exc"><span><strong>{date}</strong> : {dur}, {slot}{note}</span><input type="hidden" name="date" value="{date}"><button type="submit" class="secondary">Supprimer</button></form>"#,
                    date = escape_html(date),
                    dur = format_hm(s.available_min),
                    note = note.as_deref().map(|n| format!(" — {}", escape_html(n))).unwrap_or_default(),
                )
            })
            .collect()
    };

    let source_cfg = crate::sources::config(pool).await;
    let plugin_status = crate::sources::statuses(pool).await;
    let mut source_rows = String::new();
    for (i, id) in source_cfg.order.iter().enumerate() {
        let enabled = source_cfg.enabled.contains(id);
        let (badge, detail) = match plugin_status.get(id) {
            Some(Ok(m)) if m.configured => ("prêt".to_string(), String::new()),
            Some(Ok(m)) => ("à configurer".to_string(), m.message.clone().unwrap_or_default()),
            Some(Err(e)) => ("injoignable".to_string(), e.clone()),
            None => ("injoignable".to_string(), String::new()),
        };
        let up = if i > 0 { format!(r#"<button type="submit" name="move" value="{id}:up" class="secondary" title="Monter" aria-label="Monter">↑</button>"#) } else { String::new() };
        let down = if i + 1 < source_cfg.order.len() {
            format!(r#"<button type="submit" name="move" value="{id}:down" class="secondary" title="Descendre" aria-label="Descendre">↓</button>"#)
        } else {
            String::new()
        };
        source_rows.push_str(&format!(
            r#"<div class="source-row"><span><strong>{rank}.</strong></span>
  <label class="grow"><input type="checkbox" name="enabled" value="{id}"{checked}> {label} <span class="badge">{badge}</span>{detail}</label>
  <input type="hidden" name="order" value="{id}">{up}{down}</div>
"#,
            rank = i + 2,
            id = escape_html(id),
            label = escape_html(&crate::sources::label(id)),
            checked = if enabled { " checked" } else { "" },
            detail = if detail.is_empty() { String::new() } else { format!(r#" <span class="hint">{}</span>"#, escape_html(&detail)) },
        ));
    }
    let no_plugins = if source_cfg.order.is_empty() {
        r#"<p class="hint">Aucun plugin n'est déclaré. Un plugin est un petit service (un conteneur de plus) qui apporte les activités d'une source comme Garmin Connect : ajoutez-le à votre <code>docker-compose.yml</code>, puis déclarez-le pour CyclAI avec deux variables d'environnement, par exemple :</p>
<pre class="hint">CYCLAI_PLUGINS=garmin=http://garmin:8080
CYCLAI_PLUGIN_GARMIN_SECRET=une-longue-phrase-secrete-partagee</pre>
<p class="hint">Les identifiants de la source (clé API, etc.) se renseignent dans le conteneur du plugin, pas ici. Voir docs/PLUGINS.md.</p>"#
    } else {
        ""
    };

    let info = super::layout::info;
    let num = |id: &str, value: &dyn std::fmt::Display, attrs: &str| format!(r#"<input type="number" id="{id}" name="{id}" value="{value}" {attrs}>"#);

    let athlete_fields = [
        field("ftp_watts", &format!("{} (W)", super::annotate_acronyms("FTP")), &num("ftp_watts", &ftp_watts, r#"min="1""#), ""),
        field("hr_max", "FC max (bpm)", &num("hr_max", &hr_max, r#"min="1""#), ""),
        field("hr_rest", "FC repos (bpm)", &num("hr_rest", &hr_rest, r#"min="1""#), ""),
        field(
            "lthr",
            "FC seuil / LTHR (bpm)",
            &num("lthr", &lthr, r#"min="0""#),
            "Référence du hrTSS (une heure à la FC seuil = 100). 0 : automatique, FC repos + 85 % de la réserve cardiaque.",
        ),
        field(
            "weight_kg",
            "Poids (kg)",
            &num("weight_kg", &weight, r#"min="30" max="250" step="0.1" placeholder="non renseigné""#),
            "Sert à classer votre puissance maximale par paliers en W/kg (comme Strava) sur la page Statistiques.",
        ),
        field(
            "run_pace",
            "Allure seuil en course à pied (min/km)",
            &format!(r#"<input type="text" class="short" id="run_pace" name="run_pace" value="{run_pace}" placeholder="4:30" pattern="[0-9]+:[0-5][0-9]">"#),
            "Référence des cibles d'allure du concepteur de séances : place une étape d'allure sur l'échelle d'intensité (graphique, TSS et distance estimés). Vide : non réglée.",
        ),
        field(
            "hr_tss_coef",
            "Coefficient hrTSS dans la charge totale",
            &num("hr_tss_coef", &hr_tss_coef, r#"min="0" max="10" step="0.05""#),
            "Multiplie le hrTSS des courses quand il s'additionne au TSS puissance du vélo dans CTL/ATL/TSB et les totaux (1 = compte comme un TSS vélo, 0,8 = 20 % de moins, 0 = les courses ne comptent pas). Le hrTSS affiché sur chaque course reste le score brut.",
        ),
    ]
    .concat();

    let rules_fields = [
        field("max_ctl_ramp_per_week", "Gain de CTL maximal sur 7 jours", &num("max_ctl_ramp_per_week", &rules.max_ctl_ramp_per_week, r#"min="0" step="0.5""#), ""),
        field("min_tsb", "Plancher de TSB", &num("min_tsb", &rules.min_tsb, r#"min="-100" max="0" step="1""#), ""),
        field("intense_if", "Jour intense : IF à partir de", &num("intense_if", &rules.intense_if, r#"min="0.5" max="1.5" step="0.01""#), ""),
        field("intense_tss", "Jour intense : TSS du jour à partir de", &num("intense_tss", &rules.intense_tss, r#"min="1" step="1""#), ""),
        field("max_consecutive_intense", "Jours intenses consécutifs maximum", &num("max_consecutive_intense", &rules.max_consecutive_intense, r#"min="1" max="7""#), ""),
        field("min_rest_days_per_week", "Jours de repos minimum par fenêtre de 7 jours", &num("min_rest_days_per_week", &rules.min_rest_days_per_week, r#"min="0" max="7""#), ""),
    ]
    .concat();

    let exception_fields = [
        field("exc_date", "Date", r#"<input type="date" id="exc_date" name="date" required>"#, ""),
        field("h_exc", "Disponibilité", &avail_inputs("exc", 2 * 60, "soir"), "Durée disponible ce jour-là. « Journée libre » : vous pouvez rouler en journée comme le soir ; sinon, seulement le matin ou seulement le soir."),
        field("exc_note", "Note", r#"<input type="text" id="exc_note" name="note" maxlength="300">"#, ""),
    ]
    .concat();

    let appearance_fields = [
        field(
            "theme_color",
            "Couleur du thème",
            &format!(
                r#"<div class="swatches">{swatches}<span class="swatch-custom"><input type="color" id="theme_color" name="theme_color" value="{color}" class="color-input" title="Couleur personnalisée"><span class="hint">personnalisée</span></span></div>"#,
                swatches = THEME_PRESETS
                    .iter()
                    .map(|(name, hex)| format!(
                        r#"<button type="button" class="swatch{}" data-color="{hex}" style="background:{hex}" title="{name}" aria-label="{name}"></button>"#,
                        if hex.eq_ignore_ascii_case(&settings.theme_color) { " selected" } else { "" }
                    ))
                    .collect::<String>(),
                color = escape_html(&settings.theme_color),
            ),
            "",
        ),
        field(
            "theme_mode",
            "Mode",
            &format!(
                r#"<select id="theme_mode" name="theme_mode">{}</select>"#,
                super::layout::MODES
                    .iter()
                    .map(|(v, l)| format!(r#"<option value="{v}"{}>{l}</option>"#, if *v == settings.theme_mode { " selected" } else { "" }))
                    .collect::<String>()
            ),
            "",
        ),
    ]
    .concat();

    let overlap_field = field(
        "overlap_tolerance_s",
        "Chevauchement toléré entre deux activités (secondes)",
        &num("overlap_tolerance_s", &source_cfg.tolerance_s, r#"min="0" max="3600" step="1""#),
        "Deux activités ne sont considérées comme superposées que si elles partagent plus de ce nombre de secondes (une sortie qui enchaîne directement sur la suivante ne compte pas).",
    );
    let api_key_field = field(
        "anthropic_api_key",
        "Clé API",
        &format!(r#"<input type="password" autocomplete="off" id="anthropic_api_key" name="anthropic_api_key" value="{}">"#, escape_html(&settings.anthropic_api_key)),
        "",
    );

    let body = format!(
        r##"{error_html}
<nav class="settings-nav" aria-label="Catégories">
  <a href="#general">Général</a><a href="#entrainement">Entraînement</a><a href="#sources">Sources d'activités</a><a href="#ia">Intelligence artificielle</a>
</nav>

<div class="settings-group" id="general">
<div class="group-title" role="heading" aria-level="2">Général</div>
<section>
<h2>Apparence</h2>{info_appearance}
<form method="post" action="/settings/appearance">
  {appearance_fields}
  <script>
document.querySelectorAll('.swatch').forEach(function(b) {{
  b.addEventListener('click', function() {{
    document.querySelector('input[name=theme_color]').value = b.dataset.color;
    document.querySelectorAll('.swatch').forEach(function(x) {{ x.classList.toggle('selected', x === b); }});
  }});
}});
</script>
  <button type="submit">Enregistrer</button>
  <button type="submit" name="reset" value="1" class="secondary">Réinitialiser</button>
</form>
</section>

</div>

<div class="settings-group" id="entrainement">
<div class="group-title" role="heading" aria-level="2">Entraînement</div>
<section>
<h2>Profil physiologique</h2>{info_profile}
<form method="post" action="/settings/athlete">
  {athlete_fields}
  <button type="submit">Enregistrer</button>
</form>
</section>

<section>
<h2>Règles d'entraînement</h2>{info_rules}
<form method="post" action="/settings/rules">
  {rules_fields}
  <button type="submit">Enregistrer</button>
</form>
</section>

<section>
<h2>Disponibilités</h2>{info_availability}
<form method="post" action="/settings/availability">
{availability_rows}
  <button type="submit">Enregistrer</button>
</form>
<script>
document.querySelectorAll('.avail-free input').forEach(function(c) {{
  var tod = c.closest('.avail-ctl').querySelector('.avail-tod');
  function sync() {{ tod.hidden = c.checked; tod.querySelector('select').disabled = c.checked; }}
  c.addEventListener('change', sync);
  sync();
}});
</script>
<h3>Exceptions à venir</h3>
{availability_exceptions}
<form method="post" action="/settings/availability/exception">
  {exception_fields}
  <button type="submit">Ajouter ou remplacer l'exception</button>
</form>
</section>

</div>

<div class="settings-group" id="sources">
<div class="group-title" role="heading" aria-level="2">Sources d'activités</div>
<section>
<h2>Plugins d'activités</h2>{info_plugins}
<form method="post" action="/settings/sources">
  <div class="source-row"><span><strong>1.</strong></span><span class="grow">Fichier .fit/.gpx <span class="badge">toujours actif</span></span></div>
{source_rows}
{no_plugins}
  {overlap_field}
  <button type="submit">Enregistrer</button>
</form>
</section>
</div>

<div class="settings-group" id="ia">
<div class="group-title" role="heading" aria-level="2">Intelligence artificielle</div>
<section>
<h2>Clé API Anthropic</h2>{info_key}
<form method="post" action="/settings/keys">
  {api_key_field}
  <button type="submit">Enregistrer</button>
</form>
</section>

<section>
<h2>Profil cycliste</h2>{info_cyclist}
<form method="post" action="/settings/profile">
  <textarea class="settings-textarea" name="athlete_profile">{athlete_profile}</textarea>
  <button type="submit">Enregistrer</button>
</form>
</section>

<section>
<h2>Prompt système — bilan d'activité</h2>{info_bilan}
<form method="post" action="/settings/prompts/bilan">
  <textarea class="settings-textarea" name="system_prompt">{bilan_system_prompt}</textarea>
  <button type="submit">Enregistrer</button>
</form>
</section>

<section>
<h2>Prompt système — digest hebdomadaire</h2>{info_digest}
<form method="post" action="/settings/prompts/digest">
  <textarea class="settings-textarea" name="system_prompt">{digest_system_prompt}</textarea>
  <button type="submit">Enregistrer</button>
</form>
</section>
</div>
"##,
        error_html = error_html,
        appearance_fields = appearance_fields,
        athlete_fields = athlete_fields,
        rules_fields = rules_fields,
        exception_fields = exception_fields,
        overlap_field = overlap_field,
        api_key_field = api_key_field,
        info_appearance = info("La couleur d'accent colore la navigation, les boutons et les liens ; les couleurs des zones et des courbes (puissance, FC…) restent fixes pour rester lisibles d'un écran à l'autre."),
        info_profile = info("Enregistrer la FTP recalcule rétroactivement le TSS, l'IF et les zones de tout votre historique. La FC max, la FC repos et la FC seuil recalculent le hrTSS des courses à pied."),
        info_rules = info("Le serveur les applique aux séances proposées et aux simulations de charge : l'agent ne peut pas les contourner. Un jour est « intense » si son IF atteint le seuil ou si son TSS atteint le TSS minimum."),
        info_availability = info("Temps dont vous disposez pour rouler, par jour de la semaine. L'agent s'en sert pour dimensionner les séances ; il peut poser des exceptions (vacances, soirées longues), mais jamais modifier ces règles. « Journée libre » : vous pouvez rouler en journée comme le soir ; sinon, seulement le matin ou seulement le soir."),
        info_plugins = info("CyclAI fonctionne seul : l'import de fichiers .fit/.gpx (bouton du tableau de bord) est toujours disponible et passe en premier. Les autres sources (par exemple Garmin Connect) sont des plugins, désactivés tant que vous ne les activez pas. « Synchroniser » interroge les plugins activés dans l'ordre ci-dessous ; une activité qui chevauche une activité déjà enregistrée est ignorée, donc la source la mieux classée l'emporte. Le nouvel ordre ne joue que pour les prochaines synchronisations : pour l'appliquer à tout l'historique, il faut le réimporter."),
        info_key = info("Optionnelle : sans clé, les bilans et digests ne sont pas générés automatiquement, mais un « rapport IA » est téléchargeable depuis le tableau de bord et les pages d'activité, à coller dans Claude, ChatGPT…"),
        info_cyclist = info("Réinjecté dans chaque prompt envoyé au LLM (bilans et digest hebdomadaire) : FTP et zones, objectifs, équipement, contraintes…"),
        info_bilan = info("Envoyé au LLM à chaque génération ou régénération du bilan d'une sortie."),
        info_digest = info("Envoyé au LLM à chaque génération du digest hebdomadaire (recommandation de séance)."),
        availability_rows = availability_rows,
        availability_exceptions = availability_exceptions,
        source_rows = source_rows,
        no_plugins = no_plugins,
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
    anthropic_api_key: Option<String>,
}

pub async fn update_keys(State(state): State<AppState>, axum::extract::Form(form): axum::extract::Form<KeysForm>) -> Redirect {
    if let Some(key) = &form.anthropic_api_key {
        let _ = sqlx::query("UPDATE settings SET anthropic_api_key = ? WHERE id = 1").bind(key.trim()).execute(&state.pool).await;
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
    #[serde(default)]
    weight_kg: String,
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
        "UPDATE athlete_config SET ftp_watts = ?, hr_max = ?, hr_rest = ?, lthr = ?, hr_tss_coef = ?, run_threshold_pace_s = ?, weight_kg = ?, updated_at = datetime('now') WHERE id = 1",
    )
    .bind(form.ftp_watts)
    .bind(form.hr_max)
    .bind(form.hr_rest)
    .bind(form.lthr.max(0))
    .bind(if form.hr_tss_coef.is_finite() { form.hr_tss_coef.clamp(0.0, 10.0) } else { 1.0 })
    // Blank or unreadable = not set (0).
    .bind(crate::workout::parse_pace(&form.run_pace).filter(|p| *p > 0.0).map(|p| p.round() as i64).unwrap_or(0))
    // Blank or unreadable = not set (0).
    .bind(form.weight_kg.trim().replace(',', ".").parse::<f64>().ok().filter(|w| w.is_finite() && *w > 0.0).map(|w| w.clamp(30.0, 250.0)).unwrap_or(0.0))
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

/// One settings row: label on the left, control on the right with its own
/// hint right under it, so descriptions always line up with their field.
fn field(id: &str, label: &str, control: &str, hint: &str) -> String {
    let hint = if hint.is_empty() { String::new() } else { format!(r#"<p class="hint">{}</p>"#, escape_html(hint)) };
    format!(r#"<div class="field"><label for="{id}">{label}</label><div class="control">{control}{hint}</div></div>"#)
}

/// "2 h 30", "2 h" or "45 min".
fn format_hm(min: i64) -> String {
    match (min / 60, min % 60) {
        (0, m) => format!("{m} min"),
        (h, 0) => format!("{h} h"),
        (h, m) => format!("{h} h {m:02}"),
    }
}

/// Duration (hours + minutes), "journée libre" checkbox and, when it is not
/// ticked, a morning / evening choice. Field names end with `sfx`.
fn avail_inputs(sfx: &str, minutes: i64, tod: &str) -> String {
    let (h, m) = (minutes / 60, minutes % 60);
    let sel = |v: &str| if tod == v || (v == "soir" && tod != "matin") { " selected" } else { "" };
    format!(
        r#"<div class="avail-ctl"><span class="avail-dur"><input type="number" id="h_{sfx}" name="h_{sfx}" value="{h}" min="0" max="12" aria-label="Heures"> h <input type="number" name="m_{sfx}" value="{m}" min="0" max="59" aria-label="Minutes"> min</span><label class="avail-free"><input type="checkbox" name="free_{sfx}" value="1"{checked}> Journée libre</label><span class="avail-tod"><select name="tod_{sfx}" aria-label="Créneau"><option value="matin"{m_sel}>Matin seulement</option><option value="soir"{s_sel}>Soir seulement</option></select></span></div>"#,
        checked = if tod == "journee" { " checked" } else { "" },
        m_sel = sel("matin"),
        s_sel = sel("soir"),
    )
}

/// Hours + minutes form values as minutes, `None` when neither was sent.
fn parse_hm(h: Option<&str>, m: Option<&str>) -> Option<i64> {
    if h.is_none() && m.is_none() {
        return None;
    }
    let n = |v: Option<&str>| v.and_then(|x| x.trim().parse::<i64>().ok()).unwrap_or(0).max(0);
    Some((n(h) * 60 + n(m)).min(crate::availability::MAX_MINUTES))
}

fn slot_time_of_day(free: bool, tod: Option<&str>) -> &'static str {
    if free {
        "journee"
    } else if tod == Some("matin") {
        "matin"
    } else {
        "soir"
    }
}

/// Weekday defaults: fields `h_0`/`m_0`, `free_0`, `tod_0`… (Monday = 0). A
/// day ticked "journée libre" is available all day; otherwise morning or
/// evening only. The context stays what it was. Rows that don't validate keep
/// their current value.
pub async fn update_availability(State(state): State<AppState>, axum::extract::Form(fields): axum::extract::Form<Vec<(String, String)>>) -> Redirect {
    let get = |k: String| fields.iter().find(|(n, _)| *n == k).map(|(_, v)| v.as_str());
    let mut slots = crate::availability::defaults(&state.pool).await.unwrap_or_default();
    for (i, slot) in slots.iter_mut().enumerate() {
        let Some(available_min) = parse_hm(get(format!("h_{i}")), get(format!("m_{i}"))) else { continue };
        let candidate = crate::availability::Slot {
            available_min,
            time_of_day: slot_time_of_day(get(format!("free_{i}")).is_some(), get(format!("tod_{i}"))).to_string(),
            context: slot.context.clone(),
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
    #[serde(default)]
    h_exc: String,
    #[serde(default)]
    m_exc: String,
    #[serde(default)]
    free_exc: Option<String>,
    #[serde(default)]
    tod_exc: String,
    #[serde(default)]
    note: String,
}

pub async fn add_availability_exception(State(state): State<AppState>, axum::extract::Form(f): axum::extract::Form<AvailabilityExceptionForm>) -> Redirect {
    let slot = crate::availability::Slot {
        available_min: parse_hm(Some(&f.h_exc), Some(&f.m_exc)).unwrap_or(0),
        time_of_day: slot_time_of_day(f.free_exc.is_some(), Some(&f.tod_exc)).to_string(),
        context: "autre".to_string(),
    };
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
    let mut order = crate::sources::normalize_order(&values("order"), &crate::sources::registry::ids());
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
