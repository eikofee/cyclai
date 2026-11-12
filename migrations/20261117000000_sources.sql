-- Sources d'activités branchables (Strava, Garmin…) en plus de l'import de
-- fichiers .fit/.gpx, qui reste toujours disponible.
--
-- `source` : d'où vient l'activité ('strava', 'garmin' ou 'file'). `external_id` :
-- son identifiant chez la source (absent pour un fichier). Les id Strava restent
-- les clés primaires ; les autres sources utilisent des id négatifs.
ALTER TABLE activities ADD COLUMN source TEXT NOT NULL DEFAULT 'strava';
ALTER TABLE activities ADD COLUMN external_id TEXT;
UPDATE activities SET source = 'file' WHERE id < 0;
UPDATE activities SET external_id = CAST(id AS TEXT) WHERE id > 0;
CREATE UNIQUE INDEX idx_activities_source_external ON activities (source, external_id) WHERE external_id IS NOT NULL;

-- `source_order` : priorité entre les sources (la première passe en premier à
-- la synchronisation). `enabled_sources` : les plugins activés à la main
-- (aucun par défaut ; Strava reste activé s'il était déjà configuré).
-- `overlap_tolerance_s` : chevauchement toléré entre deux activités, en secondes.
ALTER TABLE settings ADD COLUMN garmin_api_key TEXT NOT NULL DEFAULT '';
ALTER TABLE settings ADD COLUMN source_order TEXT NOT NULL DEFAULT '["strava","garmin"]';
ALTER TABLE settings ADD COLUMN enabled_sources TEXT NOT NULL DEFAULT '[]';
ALTER TABLE settings ADD COLUMN overlap_tolerance_s INTEGER NOT NULL DEFAULT 10;
UPDATE settings SET enabled_sources = '["strava"]' WHERE strava_client_id <> '';

-- Dernière synchronisation réussie, par source (le jeton Strava reste dans sync_state).
CREATE TABLE source_sync_state (
    source          TEXT PRIMARY KEY,
    last_sync_epoch INTEGER NOT NULL DEFAULT 0
);
INSERT INTO source_sync_state (source, last_sync_epoch) SELECT 'strava', last_sync_epoch FROM sync_state WHERE id = 1;

-- Activités d'une source écartées car elles chevauchent une activité déjà
-- stockée : mémorisées pour ne pas être retéléchargées à chaque synchronisation.
CREATE TABLE ignored_activities (
    source               TEXT NOT NULL,
    external_id          TEXT NOT NULL,
    overlaps_activity_id INTEGER,
    title                TEXT,
    started_at           TEXT,
    created_at           TEXT NOT NULL DEFAULT (datetime('now')),
    PRIMARY KEY (source, external_id)
);
