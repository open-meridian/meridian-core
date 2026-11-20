-- The rows kept so far checked to name no range (step 3d; section 6.2):
-- a scan under SHARE UPDATE EXCLUSIVE, which reads and writes pass, so the
-- conversion after it attaches the table without reading it again.
ALTER TABLE lake_row VALIDATE CONSTRAINT lake_row_unpartitioned;
