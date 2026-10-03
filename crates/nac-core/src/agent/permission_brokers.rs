use super::*;

impl Agent {
    pub(crate) fn configure_permission_broker(
        &mut self,
        session_config_version: i64,
    ) -> Option<Arc<crate::permissions::PermissionBroker>> {
        if !self.direct_primary {
            return None;
        }
        if let Some(existing) = &self.tool_runtime.permission_broker {
            return Some(Arc::clone(existing));
        }
        let session_id = self.tool_runtime.session_id.clone()?;
        let backend = crate::permissions::PermissionBackend::from_execution_backend(
            self.tool_runtime.backend.as_ref(),
        );
        let broker = Arc::new(crate::permissions::PermissionBroker::new(
            self.tool_runtime.store_path.clone(),
            session_id,
            backend,
            session_config_version,
            self.permission_rules.clone(),
        ));
        self.tool_runtime.permission_broker = Some(Arc::clone(&broker));
        Some(broker)
    }

    pub(crate) fn install_claude_approval_broker(
        &mut self,
        broker: Arc<crate::claude_approval::ClaudeApprovalBroker>,
    ) {
        self.tool_runtime.claude_approval_broker = Some(broker);
    }
}
