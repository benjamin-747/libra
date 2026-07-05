-- Rollback for 2026070803_agent_audit_log (AG-24a, plan.md Task A8.5).
--
-- INTENTIONALLY NON-DESTRUCTIVE. Audit data is compliance-critical and
-- may never be dropped by a schema rollback (agent.md §合规: 删除整表/整
-- 文件须走合规审批流程). So this down does NOT `DROP TABLE` and does NOT
-- `DELETE FROM agent_audit_log`. Instead it "stops new writes" by
-- installing a BEFORE INSERT freeze trigger, while leaving every recorded
-- row and the UPDATE/DELETE-reject triggers in place. Re-applying the
-- forward migration drops this freeze trigger to re-enable writes.

CREATE TRIGGER IF NOT EXISTS agent_audit_log_frozen_after_rollback
    BEFORE INSERT ON agent_audit_log
    FOR EACH ROW
BEGIN
    SELECT RAISE(ABORT, 'agent_audit_log is frozen: the audit-log migration was rolled back; re-apply it to resume writes');
END;
