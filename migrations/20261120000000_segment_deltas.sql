-- Un passage sur un segment se compare désormais en valeur absolue : écart de
-- temps (secondes, négatif = plus rapide) et, quand les deux passages ont de la
-- puissance, écart de puissance moyenne (watts). Le record est le passage le
-- plus rapide, et la puissance n'est plus requise pour détecter un passage.
-- Les passages sont reconstruits au prochain démarrage (voir segments::refresh_if_stale).
ALTER TABLE segment_efforts ADD COLUMN delta_vs_pr_s     REAL;
ALTER TABLE segment_efforts ADD COLUMN delta_vs_pr_w     REAL;
ALTER TABLE segment_efforts ADD COLUMN delta_vs_recent_s REAL;
ALTER TABLE segment_efforts ADD COLUMN delta_vs_recent_w REAL;
ALTER TABLE segment_efforts ADD COLUMN delta_vs_last_s   REAL;
ALTER TABLE segment_efforts ADD COLUMN delta_vs_last_w   REAL;
ALTER TABLE segment_efforts DROP COLUMN delta_vs_pr_pct;
ALTER TABLE segment_efforts DROP COLUMN delta_vs_recent_avg_pct;
ALTER TABLE segment_efforts DROP COLUMN delta_vs_last_pct;
