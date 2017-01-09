use std::marker::PhantomData;
use futures::{self, Async, Future, Poll, Stream, Sink};
use futures::sync::oneshot;
use futures_state_stream::{StateStream, StreamEvent};
use stmt::ForEachRow;
use tokens::{self, TdsResponseToken, TokenRow};
use types::FromColumnData;
use {BoxableIo, SqlConnection, StmtResult, TdsError, TdsResult};

/// a query result consists of multiple query streams (amount of executed queries = amount of results)
pub struct ResultSetStream<I: BoxableIo, R: StmtResult<I>> {
    conn: Option<SqlConnection<I>>,
    receiver: Option<oneshot::Receiver<SqlConnection<I>>>,
    /// whether we already returned a result for the current resultset
    already_triggered: bool,
    done: bool,
    _marker: PhantomData<*const R>,
}

impl<I: BoxableIo, R: StmtResult<I>> ResultSetStream<I, R> {
    pub fn new(conn: SqlConnection<I>) -> ResultSetStream<I, R> {
        ResultSetStream {
            conn: Some(conn),
            receiver: None,
            already_triggered: false,
            done: false,
            _marker: PhantomData,
        }
    }
}

impl<I: BoxableIo, R: StmtResult<I>> StateStream for ResultSetStream<I, R> {
    type Item = R::Result;
    type State = SqlConnection<I>;
    type Error = TdsError;

    fn poll(&mut self) -> Poll<StreamEvent<Self::Item, Self::State>, Self::Error> {
        // attempt to receive the connection back to continue receiving further resultsets
        if self.receiver.is_some() {
            self.conn = Some(try_ready!(self.receiver.as_mut().unwrap().poll().map_err(|_| TdsError::Canceled)));
            self.receiver = None;
        }

        assert!(self.conn.is_some());

        if !self.done {
            let do_ret = match self.conn {
                None => false,
                Some(ref mut conn) => {
                    let mut inner = conn.borrow_mut();
                    try_ready!(inner.transport.poll_complete());

                    match try_ready!(inner.transport.read_token()) {
                        None => panic!("resultset: expected a token!"),
                        Some((last_pos, token)) => match token {
                            TdsResponseToken::ColMetaData(_) => {
                                self.already_triggered = true;
                                true
                            },
                            TdsResponseToken::Done(ref done) => {
                                assert!(!done.status.contains(tokens::DONE_MORE));
                                self.done = true;
                                let old = self.already_triggered;
                                self.already_triggered = false;
                                // make sure to return exactly one time for each result set
                                if !old {
                                    inner.transport.set_position(last_pos); // reinject
                                    true
                                } else {
                                    false
                                }
                            },
                            tok => panic!("resultset: unexpected token: {:?}", tok)
                        }
                    }
                }
            };
            if do_ret {
                let conn = self.conn.take().unwrap();
                let (sender, receiver) = oneshot::channel();
                self.receiver = Some(receiver);
                return Ok(Async::Ready(StreamEvent::Next(R::from_connection(conn, sender))))
            }
        }
        let conn = self.conn.take().unwrap();
        Ok(Async::Ready(StreamEvent::Done(conn)))
    }
}

impl<'a, I: BoxableIo> ResultSetStream<I, QueryStream<I>> {
    /// Only expect 1 result set (e.g. if you're only executing one query)
    /// and execute a given closure for the results of the first result set
    ///
    /// other result sets are silently ignored
    pub fn for_each_row<F>(self, f: F) -> ForEachRow<I, ResultSetStream<I, QueryStream<I>>, F>
        where F: FnMut(<QueryStream<I> as Stream>::Item) -> Result<(), TdsError>
    {
        ForEachRow::new(self, f)
    }
}

pub struct QueryStream<I: BoxableIo>(Option<ResultInner<I>>);

struct ResultInner<I: BoxableIo> {
    conn: SqlConnection<I>,
    ret_conn: oneshot::Sender<SqlConnection<I>>,
}

impl<'a, I: BoxableIo> Stream for QueryStream<I> {
    type Item = QueryRow;
    type Error = TdsError;

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        assert!(self.0.is_some());

        if let Some(ref mut inner) = self.0 {
            let mut inner = inner.conn.borrow_mut();
            try_ready!(inner.transport.poll_complete());

            loop {
                let token = try_ready!(inner.transport.read_token());
                match token {
                    None => panic!("query: expected token"),
                    Some((last_pos, token)) => match token {
                        TdsResponseToken::Row(row) => {
                            return Ok(Async::Ready(Some(QueryRow(row))));
                        },
                        // if this is the final done token, we need to reinject it for result set stream to handle it
                        TdsResponseToken::Done(ref done) if !done.status.contains(tokens::DONE_MORE) => {
                            inner.transport.set_position(last_pos);
                            break;
                        },
                        TdsResponseToken::Done(_) | TdsResponseToken::DoneInProc(_) => break,
                        x => panic!("query: unexpected token: {:?}", x),
                    }
                }
            }
        }

        let ResultInner { conn, ret_conn } = self.0.take().unwrap();
        ret_conn.complete(conn);
        Ok(Async::Ready(None))
    }
}

impl<'a, I: BoxableIo> StmtResult<I> for QueryStream<I> {
    type Result = QueryStream<I>;

    fn from_connection(conn: SqlConnection<I>, ret_conn: oneshot::Sender<SqlConnection<I>>) -> QueryStream<I> {
        QueryStream(Some(ResultInner {
            conn: conn,
            ret_conn: ret_conn,
        }))
    }
}

pub struct ExecFuture<I: BoxableIo> {
    inner: Option<ResultInner<I>>,
    /// whether only a Done token (that was previously injected) is the contents of this stream
    single_token: bool,
}

impl<I: BoxableIo> Future for ExecFuture<I> {
    /// amount of affected rows
    type Item = u64;
    type Error = TdsError;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        assert!(self.inner.is_some());

        let mut ret: u64 = 0;
        if let Some(ref mut inner) = self.inner {
            let mut inner = inner.conn.borrow_mut();
            try_ready!(inner.transport.poll_complete());

            loop {
                match try_ready!(inner.transport.read_token()) {
                    Some((last_pos, token)) => match token {
                        TdsResponseToken::Row(_) => {
                            self.single_token = false;
                        },
                        TdsResponseToken::Done(ref done) | TdsResponseToken::DoneInProc(ref done) | TdsResponseToken::DoneProc(ref done) => {
                            let final_token = match token {
                                TdsResponseToken::Done(_) | TdsResponseToken::DoneProc(_) => true,
                                _ => false
                            };
                            // if this is the final done token, we need to reinject it for result set stream to handle it
                            // (as in querying, if self.single_token it already was reinjected and would result in an infinite cycle)
                            if !done.status.contains(tokens::DONE_MORE) && self.single_token && final_token {
                                inner.transport.set_position(last_pos);
                            }
                            if done.status.contains(tokens::DONE_COUNT) {
                                ret = done.done_rows;
                            }
                            break;
                        },
                        x => panic!("exec: unexpected token: {:?}", x),
                    },
                    None =>  panic!("expected token")
                }
            }
        }

        let ResultInner { conn, ret_conn } = self.inner.take().unwrap();
        ret_conn.complete(conn);
        return Ok(Async::Ready(ret))
    }
}

impl<I: BoxableIo> StmtResult<I> for ExecFuture<I> {
    type Result = ExecFuture<I>;

    fn from_connection(conn: SqlConnection<I>, ret_conn: oneshot::Sender<SqlConnection<I>>) -> ExecFuture<I> {
        ExecFuture {
            inner: Some(ResultInner {
                conn: conn,
                ret_conn: ret_conn,
            }),
            single_token: true,
        }
    }
}

#[derive(Debug)]
pub struct QueryRow(TokenRow);

pub trait QueryIdx: Sized {
    fn to_idx(&self, row: &QueryRow) -> Option<usize>;
}

impl<'a> QueryIdx for &'a str {
    fn to_idx(&self, row: &QueryRow) -> Option<usize> {
        for (i, column) in row.0.meta.columns.iter().enumerate() {
            if &column.col_name.as_str() == self {
                return Some(i)
            }
        }
        None
    }
}

impl QueryIdx for usize {
    fn to_idx(&self, _: &QueryRow) -> Option<usize> {
        Some(*self)
    }
}

impl QueryRow {
    /// attempt to get a column's value for a given column index
    pub fn try_get<'a, I: QueryIdx, R: FromColumnData<'a>>(&'a self, idx: I) -> TdsResult<Option<R>> {
        let idx = match idx.to_idx(self) {
            Some(x) => x,
            None => return Ok(None),
        };

        let col_data = &self.0.columns[idx];
        R::from_column_data(col_data).map(Some)
    }

    /// retrieve a column's value for a given column index
    pub fn get<'a, I: QueryIdx, R: FromColumnData<'a>>(&'a self, idx: I) -> R {
        self.try_get(idx)
            .unwrap()
            .unwrap()
    }
}
