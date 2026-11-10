-- Séances de catalogue rangées en plan (group_key / group_name) > semaine
-- (subgroup), dans l'ordre d'origine (position). group_name existait déjà.
ALTER TABLE workouts ADD COLUMN group_key TEXT;     -- ex. 'build-me-up'
ALTER TABLE workouts ADD COLUMN subgroup  TEXT;     -- ex. 'Semaine 3'
ALTER TABLE workouts ADD COLUMN position  INTEGER NOT NULL DEFAULT 0;

CREATE INDEX idx_workouts_group ON workouts(category, group_key, position) WHERE category IS NOT NULL;
