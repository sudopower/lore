-- Ranking hints, not exclusion filters. Existing paths and retained first-chunk headers can
-- be classified without embedding again. A full push also catches headers not retained by
-- older chunkers and refreshes the chunker-version hash.
ALTER TABLE chunks ADD COLUMN is_test boolean NOT NULL DEFAULT false;
ALTER TABLE chunks ADD COLUMN is_generated boolean NOT NULL DEFAULT false;

WITH flags AS (
    SELECT d.id,
        lower(replace(coalesce(d.metadata->>'path', ''), E'\\', '/')) AS path,
        EXISTS (
            SELECT 1 FROM chunks first_chunk
            WHERE first_chunk.document_id = d.id AND first_chunk.ord = 0
              AND array_to_string((string_to_array(first_chunk.text, E'\n'))[1:10], E'\n')
                  ~* 'code generated[^\n]*do not edit'
        ) AS header_generated
    FROM documents d
)
UPDATE chunks c SET
    is_test = f.path ~ '(^|/)(tests|test|testdata|fixtures|mocks|__snapshots__|e2e)/'
        OR f.path ~ '(^|/)test_[^/]*\.'
        OR f.path ~ '(^|/)[^/]*(_test|\.spec)\.[^/]*$',
    is_generated = f.path ~ '(^|/)(out|gen)/'
        OR f.path ~ '(^|/)[^/]*(_generated|\.pb)\.[^/]*$'
        OR f.header_generated
FROM flags f WHERE c.document_id = f.id;
