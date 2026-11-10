-- Journal des appels d'outils MCP (qui a appelé quoi, et le résultat).
CREATE TABLE mcp_audit (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    at         TEXT NOT NULL DEFAULT (datetime('now')),
    client     TEXT NOT NULL,
    tool       TEXT NOT NULL,
    args_json  TEXT,
    is_error   INTEGER NOT NULL DEFAULT 0,
    status     INTEGER,
    code       TEXT
);

CREATE INDEX idx_mcp_audit_at ON mcp_audit(at);
