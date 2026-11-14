-- Poids du cycliste (kg) : sert à classer la puissance maximale par paliers
-- en W/kg sur /stats. 0 = non renseigné.
ALTER TABLE athlete_config ADD COLUMN weight_kg REAL NOT NULL DEFAULT 0;
