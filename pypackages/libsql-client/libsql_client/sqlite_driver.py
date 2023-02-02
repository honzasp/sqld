from typing import List, Optional, TYPE_CHECKING
import asyncio
import sqlite3

from .driver import _Driver, _RawStmt
from .errors import ClientResponseError
from .result import ResultSet, Row

if TYPE_CHECKING:
    import concurrent.futures

class _SqliteDriver(_Driver):
    _conn: sqlite3.Connection
    _conn_lock: asyncio.Lock
    _executor: Optional["concurrent.futures.ThreadPoolExecutor"]

    def __init__(
        self, path: str,
        executor: Optional["concurrent.futures.ThreadPoolExecutor"] = None,
    ) -> None:
        self._conn = sqlite3.connect(path, check_same_thread=False, isolation_level=None)
        self._conn_lock = asyncio.Lock()
        self._executor = executor

    async def batch(self, stmts: List[_RawStmt]) -> List[ResultSet]:
        async with self._conn_lock:
            loop = asyncio.get_running_loop()
            return await loop.run_in_executor(self._executor, lambda: _batch(self._conn, stmts))

    async def close(self) -> None:
        self._conn.close()

def _batch(conn: sqlite3.Connection, stmts: List[_RawStmt]) -> List[ResultSet]:
    conn.rollback()

    result_sets = []
    for stmt in stmts:
        try:
            cursor = conn.execute(stmt.sql, stmt.params)
        except sqlite3.DatabaseError as e:
            raise ClientResponseError(str(e))
        columns = tuple(descr[0] for descr in cursor.description)
        column_idxs = {name: idx for (idx, name) in enumerate(columns)}
        rows = [Row(column_idxs, row) for row in cursor.fetchall()]
        result_sets.append(ResultSet(columns, rows))

    return result_sets
