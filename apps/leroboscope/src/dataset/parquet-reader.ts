import { parquetReadObjects } from 'hyparquet';
import { compressors } from 'hyparquet-compressors';

/**
 * Read a parquet file buffer and return all rows as objects.
 */
export async function readParquetAsObjects(
  buffer: ArrayBuffer,
  columns?: string[],
): Promise<Record<string, unknown>[]> {
  return parquetReadObjects({
    file: buffer,
    columns: columns && columns.length > 0 ? columns : undefined,
    compressors,
  });
}

/**
 * Read specific rows from a parquet file (for slicing episodes).
 */
export async function readParquetSlice(
  buffer: ArrayBuffer,
  rowStart?: number,
  rowEnd?: number,
  columns?: string[],
): Promise<Record<string, unknown>[]> {
  return parquetReadObjects({
    file: buffer,
    columns: columns && columns.length > 0 ? columns : undefined,
    rowStart,
    rowEnd,
    compressors,
  });
}
