ALTER TABLE PUBLIC.accounts DROP CONSTRAINT accounts_status_check;
ALTER TABLE PUBLIC.accounts ADD CONSTRAINT accounts_status_check CHECK (status IN ('pending', 'active', 'locked', 'closed', 'trial', 'paused', 'archived'));
