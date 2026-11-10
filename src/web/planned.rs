//! Pages to schedule a session on a calendar day: `/planned/new` (blank, or
//! pre-filled from a template with `?from=<id>`, optionally for `?date=`),
//! `/planned/{id}` to edit/delete. Reuses the template designer; the planned
//! session keeps its own copy of the steps (see `planned`).

use super::workout::{builder_form_html, fetch_athlete, fit_response, page_shell, sport_select_html, FitQuery};
use crate::planned::Sport;
use crate::workout::Athlete;
use super::{escape_html, AppState};
use axum::extract::{Form, Path, Query, State};
use axum::response::{Html, IntoResponse, Redirect, Response};
use serde::Deserialize;
use serde_json::Value;

#[derive(Deserialize)]
pub struct NewQuery {
    date: Option<String>,
    from: Option<i64>,
}

/// Everything the form pages need to (re)render, including after an error.
struct FormView<'a> {
    heading: &'a str,
    action: String,
    steps_json: Option<&'a str>,
    date: &'a str,
    notes: &'a str,
    sport: Sport,
    from_workout_id: Option<i64>,
    offer_template_checkbox: bool,
    delete_id: Option<i64>,
    /// Status block (proposal to validate…) shown above the form.
    banner: &'a str,
    error: Option<&'a str>,
}

fn render_form(v: &FormView, ath: &Athlete) -> Html<String> {
    let template_checkbox = if v.offer_template_checkbox {
        r#"<label class="inline-check"><input type="checkbox" name="save_template" value="1"> Enregistrer aussi comme modèle de séance réutilisable</label>"#
    } else {
        ""
    };
    let from_field = v
        .from_workout_id
        .map(|id| format!(r#"<input type="hidden" name="from_workout_id" value="{id}">"#))
        .unwrap_or_default();
    let extra = format!(
        r#"{sport_select}
  <label>Date <input type="date" name="planned_date" value="{date}" required></label>
  <label>Notes (optionnel)
    <textarea name="notes" rows="2" placeholder="ex: sortie avec le club, à faire si la météo le permet...">{notes}</textarea>
  </label>
  {from_field}{template_checkbox}"#,
        sport_select = sport_select_html(v.sport),
        date = escape_html(v.date),
        notes = escape_html(v.notes),
    );
    let form = builder_form_html(&v.action, v.steps_json, ath, &extra);
    let error_html = v
        .error
        .map(|e| format!(r#"<p class="form-error">{}</p>"#, escape_html(e)))
        .unwrap_or_default();
    let export_html = v
        .delete_id
        .map(|id| {
            format!(
                r#"<p><a class="secondary-action" href="/planned/{id}/fit" download>Exporter pour Garmin (.fit, % FTP)</a>
<a class="secondary-action" href="/planned/{id}/fit?mode=watts" download>Garmin (.fit, watts)</a></p>"#
            )
        })
        .unwrap_or_default();
    let delete_html = v
        .delete_id
        .map(|id| {
            format!(
                r#"<form method="post" action="/planned/{id}/delete" onsubmit="return confirm('Retirer cette séance du calendrier ?');">
<button type="submit" class="danger">Retirer du calendrier</button>
</form>"#
            )
        })
        .unwrap_or_default();
    Html(page_shell(
        "Séance planifiée",
        &format!(
            r#"<a class="back-link" href="/">&larr; Retour au calendrier</a>
<section><h2>{heading}</h2>{banner}{error_html}{form}{export_html}{delete_html}</section>"#,
            heading = escape_html(v.heading),
            banner = v.banner,
        ),
    ))
}

pub async fn new_form(State(state): State<AppState>, Query(q): Query<NewQuery>) -> Html<String> {
    let ath = fetch_athlete(&state.pool).await;
    let date = q
        .date
        .as_deref()
        .and_then(crate::planned::parse_date)
        .unwrap_or_else(|| chrono::Utc::now().date_naive())
        .to_string();

    let template: Option<(i64, String, String)> = match q.from {
        Some(id) => sqlx::query_as("SELECT id, steps_json, sport FROM workouts WHERE id = ?")
            .bind(id)
            .fetch_optional(&state.pool)
            .await
            .ok()
            .flatten(),
        None => None,
    };
    let (from_id, steps, sport) = match &template {
        Some((id, steps, sport)) => (Some(*id), Some(steps.as_str()), Sport::from_db(sport)),
        None => (None, None, Sport::default()),
    };
    render_form(
        &FormView {
            heading: if template.is_some() { "Planifier une séance (copie du modèle)" } else { "Nouvelle séance du jour" },
            action: "/planned".to_string(),
            steps_json: steps,
            date: &date,
            notes: "",
            sport,
            from_workout_id: from_id,
            offer_template_checkbox: template.is_none(),
            delete_id: None,
            banner: "",
            error: None,
        },
        &ath,
    )
}

#[derive(Deserialize)]
pub struct PlannedForm {
    steps_json: String,
    planned_date: String,
    #[serde(default)]
    notes: String,
    #[serde(default)]
    sport: String,
    #[serde(default)]
    save_template: String,
    #[serde(default)]
    from_workout_id: String,
}

/// Validates the posted form into (date, prepared workout), or an error message.
fn read_form(form: &PlannedForm, ath: &Athlete) -> Result<(chrono::NaiveDate, crate::planned::Prepared, Sport), String> {
    let date = crate::planned::parse_date(&form.planned_date).ok_or_else(|| "Date invalide.".to_string())?;
    let value: Value = serde_json::from_str(&form.steps_json).map_err(|e| format!("JSON invalide : {e}"))?;
    let sport = Sport::parse(&form.sport).unwrap_or_default();
    let prepared = crate::planned::prepare(&value, ath, sport)?;
    Ok((date, prepared, sport))
}

pub async fn create(State(state): State<AppState>, Form(form): Form<PlannedForm>) -> Response {
    let ath = fetch_athlete(&state.pool).await;
    let (date, prepared, sport) = match read_form(&form, &ath) {
        Ok(ok) => ok,
        Err(err) => {
            let from_id = form.from_workout_id.parse().ok();
            return render_form(
                &FormView {
                    heading: "Corriger la séance",
                    action: "/planned".to_string(),
                    steps_json: Some(&form.steps_json),
                    date: &form.planned_date,
                    notes: &form.notes,
                    sport: Sport::parse(&form.sport).unwrap_or_default(),
                    from_workout_id: from_id,
                    offer_template_checkbox: from_id.is_none(),
                    delete_id: None,
                    banner: "",
                    error: Some(&err),
                },
                &ath,
            )
            .into_response();
        }
    };

    let mut template_id: Option<i64> = form.from_workout_id.parse().ok();
    if !form.save_template.is_empty() {
        template_id = sqlx::query("INSERT INTO workouts (name, steps_json, source, sport) VALUES (?, ?, 'manual', ?)")
            .bind(&prepared.name)
            .bind(&prepared.steps_json)
            .bind(sport.as_str())
            .execute(&state.pool)
            .await
            .ok()
            .map(|r| r.last_insert_rowid());
    }
    if let Err(err) = crate::planned::insert(&state.pool, date, &prepared, Some(form.notes.as_str()), template_id, "manual", sport).await {
        tracing::error!(?err, "failed to store planned workout");
    }
    Redirect::to("/").into_response()
}

/// Status line of a session, with the actions its status allows: a proposal
/// from an agent is confirmed or rejected here; upcoming sessions can be skipped.
fn status_banner(p: &crate::planned::Planned, id: i64) -> String {
    let label = match p.status.as_str() {
        "proposed" => "Proposée par un agent — à valider",
        "confirmed" => "Confirmée",
        "done" => "Réalisée",
        "skipped" => "Passée",
        "rejected" => "Refusée",
        _ => return String::new(),
    };
    let rationale = p
        .rationale
        .as_deref()
        .map(|r| format!(r#"<p class="hint">{}</p>"#, escape_html(r)))
        .unwrap_or_default();
    let button = |to: &str, text: &str, class: &str| {
        format!(
            r#"<form method="post" action="/planned/{id}/status" style="display:inline"><input type="hidden" name="status" value="{to}"><button type="submit" class="{class}">{text}</button></form> "#
        )
    };
    let mut actions = String::new();
    if crate::planned::can_transition(&p.status, "confirmed") {
        actions.push_str(&button("confirmed", "Confirmer", ""));
    }
    if crate::planned::can_transition(&p.status, "rejected") {
        actions.push_str(&button("rejected", "Refuser", "secondary"));
    }
    if crate::planned::can_transition(&p.status, "skipped") {
        actions.push_str(&button("skipped", "Passer cette séance", "secondary"));
    }
    format!(r#"<div style="margin:0 0 1rem;padding:.8rem 1rem;border-left:4px solid currentColor;border-radius:8px;background:rgba(127,127,127,.12)"><strong>{label}</strong>{rationale}<p>{actions}</p></div>"#)
}

#[derive(Deserialize)]
pub struct StatusForm {
    status: String,
}

/// `POST /planned/{id}/status` — confirm/reject a proposal, or skip a session.
pub async fn set_status(State(state): State<AppState>, Path(id): Path<i64>, Form(form): Form<StatusForm>) -> Redirect {
    if !["confirmed", "rejected", "skipped"].contains(&form.status.as_str()) {
        return Redirect::to(&format!("/planned/{id}"));
    }
    match crate::planned::set_status(&state.pool, id, &form.status).await {
        Ok(true) => Redirect::to("/"),
        Ok(false) => Redirect::to(&format!("/planned/{id}")),
        Err(err) => {
            tracing::error!(?err, planned_id = id, "failed to change planned workout status");
            Redirect::to(&format!("/planned/{id}"))
        }
    }
}

/// A free ride has no steps to edit: it shows its budget, its status and the
/// ride it was matched with.
fn free_ride_page(p: &crate::planned::Planned, id: i64, banner: &str) -> Html<String> {
    let intent = p.intent.as_deref().unwrap_or("endurance");
    let activity = p
        .activity_id
        .map(|a| format!(r#"<p><a href="/activity/{a}">Voir la sortie réalisée</a></p>"#))
        .unwrap_or_default();
    let notes = p.notes.as_deref().map(|n| format!("<p>{}</p>", escape_html(n))).unwrap_or_default();
    Html(page_shell(
        "Sortie libre",
        &format!(
            r#"<a class="back-link" href="/">&larr; Retour au calendrier</a>
<section><h2>Sortie libre du {date}</h2>{banner}
<p>Intention : <strong>{intent}</strong> · durée maximale : <strong>{duration} min</strong> · budget : <strong>{lo:.0} à {hi:.0} TSS</strong></p>
<p class="hint">Pas de structure imposée : roule librement dans ce budget. La charge réelle remplace l'estimation quand la sortie est synchronisée depuis Strava.</p>
{notes}{activity}
<form method="post" action="/planned/{id}/delete" onsubmit="return confirm('Retirer cette séance du calendrier ?');">
<button type="submit" class="danger">Retirer du calendrier</button>
</form></section>"#,
            date = escape_html(&p.planned_date),
            intent = crate::planned::intent_label(intent),
            duration = p.budget_duration_max_min.unwrap_or(0),
            lo = p.budget_tss_min.unwrap_or(0.0),
            hi = p.budget_tss_max.unwrap_or(0.0),
        ),
    ))
}

pub async fn edit_form(State(state): State<AppState>, Path(id): Path<i64>) -> Response {
    let ath = fetch_athlete(&state.pool).await;
    let Ok(Some(p)) = crate::planned::get(&state.pool, id).await else {
        return Html("<p>Séance planifiée introuvable. <a href=\"/\">Retour</a></p>".to_string()).into_response();
    };
    let banner = status_banner(&p, id);
    if p.is_free_ride() {
        return free_ride_page(&p, id, &banner).into_response();
    }
    render_form(
        &FormView {
            heading: &format!("Séance du {}", p.planned_date),
            action: format!("/planned/{id}"),
            steps_json: Some(&p.steps_json),
            date: &p.planned_date,
            notes: p.notes.as_deref().unwrap_or(""),
            sport: Sport::from_db(&p.sport),
            from_workout_id: None,
            offer_template_checkbox: false,
            delete_id: Some(id),
            banner: &banner,
            error: None,
        },
        &ath,
    )
    .into_response()
}

pub async fn update(State(state): State<AppState>, Path(id): Path<i64>, Form(form): Form<PlannedForm>) -> Response {
    let ath = fetch_athlete(&state.pool).await;
    match read_form(&form, &ath) {
        Ok((date, prepared, sport)) => {
            if let Err(err) = crate::planned::update(&state.pool, id, date, &prepared, Some(form.notes.as_str()), sport).await {
                tracing::error!(?err, planned_id = id, "failed to update planned workout");
            }
            Redirect::to("/").into_response()
        }
        Err(err) => render_form(
            &FormView {
                heading: "Corriger la séance",
                action: format!("/planned/{id}"),
                steps_json: Some(&form.steps_json),
                date: &form.planned_date,
                notes: &form.notes,
                sport: Sport::parse(&form.sport).unwrap_or_default(),
                from_workout_id: None,
                offer_template_checkbox: false,
                delete_id: Some(id),
                banner: "",
                error: Some(&err),
            },
            &ath,
        )
        .into_response(),
    }
}

pub async fn delete(State(state): State<AppState>, Path(id): Path<i64>) -> Redirect {
    if let Err(err) = crate::planned::delete(&state.pool, id).await {
        tracing::error!(?err, planned_id = id, "failed to delete planned workout");
    }
    Redirect::to("/")
}

/// `GET /planned/{id}/fit` — the planned session as a Garmin FIT workout.
pub async fn export_fit(State(state): State<AppState>, Path(id): Path<i64>, Query(q): Query<FitQuery>) -> Response {
    let Ok(Some(p)) = crate::planned::get(&state.pool, id).await else {
        return axum::http::StatusCode::NOT_FOUND.into_response();
    };
    let Ok(workout) = serde_json::from_str::<crate::workout::Workout>(&p.steps_json) else {
        return axum::http::StatusCode::UNPROCESSABLE_ENTITY.into_response();
    };
    let ath = fetch_athlete(&state.pool).await;
    fit_response(&workout, Sport::from_db(&p.sport), &ath, &q)
}
