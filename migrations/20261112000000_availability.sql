-- Availability for rides: a default per weekday (1 = Monday … 7 = Sunday)
-- plus per-date exceptions (holidays, long summer evenings, remote-work days).
CREATE TABLE availability_defaults (
    weekday INTEGER PRIMARY KEY CHECK (weekday BETWEEN 1 AND 7),
    available_min INTEGER NOT NULL,
    time_of_day TEXT NOT NULL,
    context TEXT NOT NULL
);

INSERT INTO availability_defaults (weekday, available_min, time_of_day, context) VALUES
    (1, 90, 'soir', 'travail'),
    (2, 90, 'soir', 'travail'),
    (3, 90, 'soir', 'travail'),
    (4, 90, 'soir', 'travail'),
    (5, 90, 'soir', 'travail'),
    (6, 180, 'journee', 'autre'),
    (7, 180, 'journee', 'autre');

CREATE TABLE availability_exceptions (
    date TEXT PRIMARY KEY,
    available_min INTEGER NOT NULL,
    time_of_day TEXT NOT NULL,
    context TEXT NOT NULL,
    note TEXT,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
);
