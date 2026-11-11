-- Alternatives proposées avec une séance (instantané : id, nom, durée, TSS, note).
ALTER TABLE planned_workouts ADD COLUMN alternatives_json TEXT;
