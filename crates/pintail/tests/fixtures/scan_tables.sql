CREATE TABLE big AS SELECT i::BIGINT AS i, 'value ' || (i % 10) AS s, ((i // 100) * 1.5)::DOUBLE AS f, CASE WHEN i % 3 = 0 THEN NULL ELSE i END AS some_null, NULL::INTEGER AS all_null, (i % 100)::TINYINT AS tiny, i::UINTEGER AS ui, (i // 100)::FLOAT AS fl, i::HUGEINT * 1000000000000 AS h, (i // 10)::DECIMAL(18,4) AS dec, DATE '2000-01-01' + (i // 100)::INT AS d, TIMESTAMP '2000-01-01' + INTERVAL (i) SECOND AS ts, TIME '00:00:00' + INTERVAL (i % 86400) SECOND AS t, i % 2 = 0 AS b FROM range(130000) r(i);

CREATE TABLE holes AS SELECT i, 'hole ' || (i % 20) AS s FROM range(130000) r(i);

DELETE FROM holes WHERE i % 5 = 0 OR i BETWEEN 30000 AND 50000;

CREATE TABLE odd_floats (f DOUBLE, r FLOAT, k INTEGER);

INSERT INTO odd_floats SELECT CASE WHEN i % 2 = 0 THEN (i // 100)::DOUBLE WHEN i % 3 = 0 THEN 'NaN'::DOUBLE WHEN i % 5 = 0 THEN 'Infinity'::DOUBLE ELSE -1 * (i // 100) END, CASE WHEN i % 7 = 0 THEN 'NaN'::FLOAT ELSE (i // 100) END, i FROM range(130000) r(i);

CREATE TABLE gone AS SELECT i FROM range(5000) r(i);

DELETE FROM gone;

CREATE TABLE sorted_strings AS SELECT lpad((i // 10)::VARCHAR, 8, '0') AS s, i FROM range(130000) r(i);

CHECKPOINT;
