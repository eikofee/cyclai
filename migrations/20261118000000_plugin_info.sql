-- Ce que chaque plugin d'activités dit de lui (manifeste) et dont le reste de
-- l'application a besoin : son nom et le lien vers une activité sur son site.
-- Conservé en base pour rester disponible quand le plugin est arrêté.
CREATE TABLE plugin_info (
    source             TEXT PRIMARY KEY,
    label              TEXT NOT NULL,
    activity_url       TEXT,
    activity_url_label TEXT,
    updated_at         TEXT NOT NULL DEFAULT (datetime('now'))
);
