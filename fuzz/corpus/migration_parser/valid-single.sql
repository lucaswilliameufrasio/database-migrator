-- dbm:dialect postgres
-- dbm:transaction never
-- dbm:up
DO $body$
BEGIN
    PERFORM 'not a section; -- dbm:down';
END;
$body$;
-- dbm:down
DROP TABLE IF EXISTS sample;
