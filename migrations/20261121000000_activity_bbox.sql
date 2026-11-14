ALTER TABLE activities ADD COLUMN bbox_min_lat REAL;
ALTER TABLE activities ADD COLUMN bbox_min_lng REAL;
ALTER TABLE activities ADD COLUMN bbox_max_lat REAL;
ALTER TABLE activities ADD COLUMN bbox_max_lng REAL;
ALTER TABLE activities ADD COLUMN bbox_computed INTEGER NOT NULL DEFAULT 0;
