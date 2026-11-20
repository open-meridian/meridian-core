-- lake_row partitioned (step 3d; spec/a-deployment-scales-out-and-fails-
-- over-by-configuration section 6.2): LIST by partition, then RANGE by
-- month of valid time, converted online. First, the rows kept so far are
-- said to name no range -- what lets them stand, as they are, as the new
-- table's default partition until they are copied behind. NOT VALID: no
-- scan under this migration's lock; the next validates it under a lock that
-- lets reads and writes go on.
ALTER TABLE lake_row ADD CONSTRAINT lake_row_unpartitioned CHECK (partition IS NULL) NOT VALID;
