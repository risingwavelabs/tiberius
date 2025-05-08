use crate::{error::Error, sql_read_bytes::SqlReadBytes, ColumnData};

pub(crate) async fn decode<R>(src: &mut R, len: u8) -> crate::Result<ColumnData<'static>>
where
    R: SqlReadBytes + Unpin,
{
    let res = match len {
        0 => ColumnData::I64(None),
        4 => ColumnData::I64(Some(src.read_i32_le().await? as i64 / 1e4 as i64)),
        8 => ColumnData::I64(Some({
            let high = src.read_i32_le().await? as i64;
            let low = src.read_u32_le().await? as i64;

            (high << 32) + low / 1e4 as i64
        })),
        _ => {
            return Err(Error::Protocol(
                format!("money: length of {} is invalid", len).into(),
            ));
        }
    };

    Ok(res)
}
