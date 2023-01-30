use crate::entities::DatabaseViewLayout;
use crate::services::grid_editor::{DatabaseRevisionEditor, GridRevisionMergeable};
use crate::services::persistence::block_index::BlockIndexCache;
use crate::services::persistence::kv::GridKVPersistence;
use crate::services::persistence::migration::GridMigration;
use crate::services::persistence::rev_sqlite::{SQLiteGridRevisionPersistence, SQLiteGridRevisionSnapshotPersistence};
use crate::services::persistence::GridDatabase;
use crate::services::view_editor::make_grid_view_rev_manager;
use bytes::Bytes;
use flowy_client_sync::client_grid::{make_database_operations, make_grid_block_operations, make_grid_view_operations};
use flowy_error::{FlowyError, FlowyResult};
use flowy_revision::{RevisionManager, RevisionPersistence, RevisionPersistenceConfiguration, RevisionWebSocket};
use flowy_sqlite::ConnectionPool;
use grid_model::{BuildGridContext, DatabaseRevision, DatabaseViewRevision};
use lib_infra::async_trait::async_trait;
use lib_infra::ref_map::{RefCountHashMap, RefCountValue};
use revision_model::Revision;

use crate::services::block_manager::make_grid_block_rev_manager;
use flowy_task::TaskDispatcher;
use std::sync::Arc;
use tokio::sync::RwLock;

pub trait DatabaseUser: Send + Sync {
    fn user_id(&self) -> Result<String, FlowyError>;
    fn token(&self) -> Result<String, FlowyError>;
    fn db_pool(&self) -> Result<Arc<ConnectionPool>, FlowyError>;
}

pub struct DatabaseManager {
    grid_editors: RwLock<RefCountHashMap<Arc<DatabaseRevisionEditor>>>,
    grid_user: Arc<dyn DatabaseUser>,
    block_index_cache: Arc<BlockIndexCache>,
    #[allow(dead_code)]
    kv_persistence: Arc<GridKVPersistence>,
    task_scheduler: Arc<RwLock<TaskDispatcher>>,
    migration: GridMigration,
}

impl DatabaseManager {
    pub fn new(
        grid_user: Arc<dyn DatabaseUser>,
        _rev_web_socket: Arc<dyn RevisionWebSocket>,
        task_scheduler: Arc<RwLock<TaskDispatcher>>,
        database: Arc<dyn GridDatabase>,
    ) -> Self {
        let grid_editors = RwLock::new(RefCountHashMap::new());
        let kv_persistence = Arc::new(GridKVPersistence::new(database.clone()));
        let block_index_cache = Arc::new(BlockIndexCache::new(database.clone()));
        let migration = GridMigration::new(grid_user.clone(), database);
        Self {
            grid_editors,
            grid_user,
            kv_persistence,
            block_index_cache,
            task_scheduler,
            migration,
        }
    }

    pub async fn initialize_with_new_user(&self, _user_id: &str, _token: &str) -> FlowyResult<()> {
        Ok(())
    }

    pub async fn initialize(&self, _user_id: &str, _token: &str) -> FlowyResult<()> {
        Ok(())
    }

    #[tracing::instrument(level = "debug", skip_all, err)]
    pub async fn create_grid<T: AsRef<str>>(&self, grid_id: T, revisions: Vec<Revision>) -> FlowyResult<()> {
        let grid_id = grid_id.as_ref();
        let db_pool = self.grid_user.db_pool()?;
        let rev_manager = self.make_database_rev_manager(grid_id, db_pool)?;
        rev_manager.reset_object(revisions).await?;

        Ok(())
    }

    #[tracing::instrument(level = "debug", skip_all, err)]
    async fn create_grid_view<T: AsRef<str>>(&self, view_id: T, revisions: Vec<Revision>) -> FlowyResult<()> {
        let view_id = view_id.as_ref();
        let rev_manager = make_grid_view_rev_manager(&self.grid_user, view_id).await?;
        rev_manager.reset_object(revisions).await?;
        Ok(())
    }

    #[tracing::instrument(level = "debug", skip_all, err)]
    pub async fn create_grid_block<T: AsRef<str>>(&self, block_id: T, revisions: Vec<Revision>) -> FlowyResult<()> {
        let block_id = block_id.as_ref();
        let rev_manager = make_grid_block_rev_manager(&self.grid_user, block_id)?;
        rev_manager.reset_object(revisions).await?;
        Ok(())
    }

    pub async fn open_database<T: AsRef<str>>(&self, database_id: T) -> FlowyResult<Arc<DatabaseRevisionEditor>> {
        let database_id = database_id.as_ref();
        let _ = self.migration.run_v1_migration(database_id).await;
        self.get_or_create_database_editor(database_id).await
    }

    #[tracing::instrument(level = "debug", skip_all, fields(database_id), err)]
    pub async fn close_database<T: AsRef<str>>(&self, database_id: T) -> FlowyResult<()> {
        let database_id = database_id.as_ref();
        tracing::Span::current().record("database_id", database_id);
        self.grid_editors.write().await.remove(database_id).await;
        Ok(())
    }

    // #[tracing::instrument(level = "debug", skip(self), err)]
    pub async fn get_database_editor(&self, database_id: &str) -> FlowyResult<Arc<DatabaseRevisionEditor>> {
        let read_guard = self.grid_editors.read().await;
        let editor = read_guard.get(database_id);
        match editor {
            None => {
                // Drop the read_guard ASAP in case of the following read/write lock
                drop(read_guard);
                self.open_database(database_id).await
            }
            Some(editor) => Ok(editor),
        }
    }

    async fn get_or_create_database_editor(&self, database_id: &str) -> FlowyResult<Arc<DatabaseRevisionEditor>> {
        if let Some(editor) = self.grid_editors.read().await.get(database_id) {
            return Ok(editor);
        }

        let mut database_editors = self.grid_editors.write().await;
        let db_pool = self.grid_user.db_pool()?;
        let editor = self.make_database_rev_editor(database_id, db_pool).await?;
        tracing::trace!("Open grid: {}", database_id);
        database_editors.insert(database_id.to_string(), editor.clone());
        Ok(editor)
    }

    #[tracing::instrument(level = "trace", skip(self, pool), err)]
    async fn make_database_rev_editor(
        &self,
        database_id: &str,
        pool: Arc<ConnectionPool>,
    ) -> Result<Arc<DatabaseRevisionEditor>, FlowyError> {
        let user = self.grid_user.clone();
        let rev_manager = self.make_database_rev_manager(database_id, pool.clone())?;
        let database_editor = DatabaseRevisionEditor::new(
            database_id,
            user,
            rev_manager,
            self.block_index_cache.clone(),
            self.task_scheduler.clone(),
        )
        .await?;
        Ok(database_editor)
    }

    #[tracing::instrument(level = "trace", skip(self, pool), err)]
    pub fn make_database_rev_manager(
        &self,
        database_id: &str,
        pool: Arc<ConnectionPool>,
    ) -> FlowyResult<RevisionManager<Arc<ConnectionPool>>> {
        let user_id = self.grid_user.user_id()?;

        // Create revision persistence
        let disk_cache = SQLiteGridRevisionPersistence::new(&user_id, pool.clone());
        let configuration = RevisionPersistenceConfiguration::new(6, false);
        let rev_persistence = RevisionPersistence::new(&user_id, database_id, disk_cache, configuration);

        // Create snapshot persistence
        let snapshot_object_id = format!("grid:{}", database_id);
        let snapshot_persistence = SQLiteGridRevisionSnapshotPersistence::new(&snapshot_object_id, pool);

        let rev_compress = GridRevisionMergeable();
        let rev_manager = RevisionManager::new(
            &user_id,
            database_id,
            rev_persistence,
            rev_compress,
            snapshot_persistence,
        );
        Ok(rev_manager)
    }
}

pub async fn make_database_view_data(
    _user_id: &str,
    view_id: &str,
    layout: DatabaseViewLayout,
    grid_manager: Arc<DatabaseManager>,
    build_context: BuildGridContext,
) -> FlowyResult<Bytes> {
    let BuildGridContext {
        field_revs,
        block_metas,
        blocks,
        grid_view_revision_data,
    } = build_context;

    for block_meta_data in &blocks {
        let block_id = &block_meta_data.block_id;
        // Indexing the block's rows
        block_meta_data.rows.iter().for_each(|row| {
            let _ = grid_manager.block_index_cache.insert(&row.block_id, &row.id);
        });

        // Create grid's block
        let grid_block_delta = make_grid_block_operations(block_meta_data);
        let block_delta_data = grid_block_delta.json_bytes();
        let revision = Revision::initial_revision(block_id, block_delta_data);
        grid_manager.create_grid_block(&block_id, vec![revision]).await?;
    }

    // Will replace the grid_id with the value returned by the gen_grid_id()
    let grid_id = view_id.to_owned();
    let grid_rev = DatabaseRevision::from_build_context(&grid_id, field_revs, block_metas);

    // Create grid
    let grid_rev_delta = make_database_operations(&grid_rev);
    let grid_rev_delta_bytes = grid_rev_delta.json_bytes();
    let revision = Revision::initial_revision(&grid_id, grid_rev_delta_bytes.clone());
    grid_manager.create_grid(&grid_id, vec![revision]).await?;

    // Create grid view
    let grid_view = if grid_view_revision_data.is_empty() {
        DatabaseViewRevision::new(grid_id, view_id.to_owned(), layout.into())
    } else {
        DatabaseViewRevision::from_json(grid_view_revision_data)?
    };
    let grid_view_delta = make_grid_view_operations(&grid_view);
    let grid_view_delta_bytes = grid_view_delta.json_bytes();
    let revision = Revision::initial_revision(view_id, grid_view_delta_bytes);
    grid_manager.create_grid_view(view_id, vec![revision]).await?;

    Ok(grid_rev_delta_bytes)
}

#[async_trait]
impl RefCountValue for DatabaseRevisionEditor {
    async fn did_remove(&self) {
        self.close().await;
    }
}
