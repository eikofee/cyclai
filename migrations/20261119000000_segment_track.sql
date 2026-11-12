-- Tracé d'un segment importé depuis un fichier GPX (latlng, distance, altitude,
-- au format des streams d'activité, un point tous les ~5 m) : la carte, le profil
-- et la pente s'affichent sans qu'un passage ait déjà été détecté. Les segments
-- créés avant n'en ont pas et gardent le tracé d'un de leurs passages.
ALTER TABLE reference_segments ADD COLUMN track_json TEXT;
