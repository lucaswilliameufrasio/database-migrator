CREATE TABLE `semi;colon` (value VARCHAR(40) DEFAULT 'a;b');
/*!80000 SELECT 1; */
SELECT `semi;colon`.`value` FROM `semi;colon`;
