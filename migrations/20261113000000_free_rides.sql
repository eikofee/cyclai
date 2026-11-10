-- Free rides: a session without a structured workout, only a budget (TSS
-- range, max duration, intent) that the engine bounds. `activity_id` links a
-- session that has been done to the ride it was matched with.
ALTER TABLE planned_workouts ADD COLUMN kind                    TEXT NOT NULL DEFAULT 'workout';
ALTER TABLE planned_workouts ADD COLUMN budget_tss_min          REAL;
ALTER TABLE planned_workouts ADD COLUMN budget_tss_max          REAL;
ALTER TABLE planned_workouts ADD COLUMN budget_duration_max_min INTEGER;
ALTER TABLE planned_workouts ADD COLUMN intent                  TEXT;
ALTER TABLE planned_workouts ADD COLUMN activity_id             INTEGER;
