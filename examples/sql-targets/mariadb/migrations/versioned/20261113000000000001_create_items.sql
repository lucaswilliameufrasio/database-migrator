-- dbm:dialect mysql
-- dbm:transaction never
-- dbm:up
CREATE TABLE items (
    id BIGINT NOT NULL PRIMARY KEY,
    name VARCHAR(255) NOT NULL
);

-- dbm:down
DROP TABLE items;
