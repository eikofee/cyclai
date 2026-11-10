-- Séances importées (catalogue Zwift) : rangées par catégorie / groupe,
-- identifiées par external_id pour être remises à jour sans doublon, et en
-- lecture seule dans l'interface (source <> 'manual' / 'digest').
ALTER TABLE workouts ADD COLUMN category    TEXT;     -- ex. 'Depuis Zwift'
ALTER TABLE workouts ADD COLUMN group_name  TEXT;     -- ex. 'FTP Builder — Semaine 1'
ALTER TABLE workouts ADD COLUMN description TEXT;
ALTER TABLE workouts ADD COLUMN source_url  TEXT;
ALTER TABLE workouts ADD COLUMN external_id TEXT;

CREATE UNIQUE INDEX idx_workouts_external_id ON workouts(external_id) WHERE external_id IS NOT NULL;
