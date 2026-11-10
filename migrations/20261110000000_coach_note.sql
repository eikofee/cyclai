-- Note du coach ("MotD") : un court message laissé par un agent via l'API et
-- affiché en haut du dashboard. Une seule ligne ; absente = pas de note.
CREATE TABLE coach_note (
    id         INTEGER PRIMARY KEY CHECK (id = 1),
    message    TEXT NOT NULL,
    author     TEXT,
    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
);
