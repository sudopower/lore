-- The exact text of a document that came in through the documents API. Chunks are cut and
-- overlap, so they cannot give back the original (long lines, blank lines, CRLF), and GET returns
-- the text as it was sent. Repo files and sessions leave it NULL and are rebuilt from their chunks.
ALTER TABLE documents ADD COLUMN body text;
