-- Règles d'entraînement appliquées par le serveur (src/rules.rs), réglables
-- depuis /settings.
ALTER TABLE athlete_config ADD COLUMN max_ctl_ramp_per_week  REAL    NOT NULL DEFAULT 6;    -- CTL gagné au plus sur 7 jours
ALTER TABLE athlete_config ADD COLUMN min_tsb                REAL    NOT NULL DEFAULT -25;  -- plancher de TSB
ALTER TABLE athlete_config ADD COLUMN intense_if             REAL    NOT NULL DEFAULT 0.85; -- jour intense si IF >= ...
ALTER TABLE athlete_config ADD COLUMN intense_tss            REAL    NOT NULL DEFAULT 100;  -- ... ou TSS du jour >= ...
ALTER TABLE athlete_config ADD COLUMN max_consecutive_intense INTEGER NOT NULL DEFAULT 2;
ALTER TABLE athlete_config ADD COLUMN min_rest_days_per_week INTEGER NOT NULL DEFAULT 1;

-- Cycle de vie d'une séance planifiée : planned | proposed | confirmed | done
-- | skipped | rejected. `proposed` = suggérée par un agent, à valider dans l'UI.
ALTER TABLE planned_workouts ADD COLUMN status          TEXT NOT NULL DEFAULT 'planned';
ALTER TABLE planned_workouts ADD COLUMN rationale       TEXT;
ALTER TABLE planned_workouts ADD COLUMN idempotency_key TEXT;
CREATE UNIQUE INDEX idx_planned_idempotency ON planned_workouts(idempotency_key) WHERE idempotency_key IS NOT NULL;

-- Ressenti subjectif du jour (un par date), saisi via l'API.
CREATE TABLE checkins (
    date        TEXT PRIMARY KEY,                                   -- YYYY-MM-DD
    legs        INTEGER NOT NULL CHECK (legs BETWEEN 1 AND 5),
    motivation  INTEGER NOT NULL CHECK (motivation BETWEEN 1 AND 5),
    rhr_bpm     INTEGER,
    note        TEXT,
    weather     TEXT,
    created_at  TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at  TEXT NOT NULL DEFAULT (datetime('now'))
);
