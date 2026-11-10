-- OAuth 2.1 pour le connecteur MCP (Claude web/mobile).
-- Les secrets (codes, jetons) ne sont jamais stockés en clair : seulement leur SHA-256.

CREATE TABLE oauth_settings (
    id             INTEGER PRIMARY KEY CHECK (id = 1),
    password_hash  TEXT,                              -- argon2, défini via `cyclai oauth set-password`
    refresh_days   INTEGER NOT NULL DEFAULT 90        -- inactivité max d'un refresh token
);
INSERT INTO oauth_settings (id) VALUES (1);

CREATE TABLE oauth_clients (
    client_id           TEXT PRIMARY KEY,
    client_name         TEXT,
    redirect_uris_json  TEXT NOT NULL,
    created_at          TEXT NOT NULL DEFAULT (datetime('now')),
    revoked_at          TEXT
);

CREATE TABLE oauth_codes (
    code_hash       TEXT PRIMARY KEY,
    client_id       TEXT NOT NULL,
    redirect_uri    TEXT NOT NULL,
    code_challenge  TEXT NOT NULL,
    scope           TEXT NOT NULL,
    expires_at      INTEGER NOT NULL                  -- epoch, secondes
);

CREATE TABLE oauth_access_tokens (
    token_hash  TEXT PRIMARY KEY,
    client_id   TEXT NOT NULL,
    scope       TEXT NOT NULL,
    expires_at  INTEGER NOT NULL,
    created_at  TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE TABLE oauth_refresh_tokens (
    token_hash    TEXT PRIMARY KEY,
    client_id     TEXT NOT NULL,
    scope         TEXT NOT NULL,
    created_at    TEXT NOT NULL DEFAULT (datetime('now')),
    last_used_at  TEXT,
    expires_at    INTEGER NOT NULL,                   -- glisse à chaque rotation
    revoked_at    TEXT
);

CREATE INDEX idx_oauth_refresh_client ON oauth_refresh_tokens(client_id);
CREATE INDEX idx_oauth_access_client ON oauth_access_tokens(client_id);

-- Mots de passe erronés sur la page de consentement (verrou après 5 échecs en 15 min).
CREATE TABLE oauth_failures (at INTEGER NOT NULL);
