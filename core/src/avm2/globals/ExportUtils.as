package {
    import flash.utils.ByteArray;

    // Native, high-throughput export of grid-style data, returned as a
    // ByteArray. The work happens in Rust (see `export_utils.rs`) straight
    // over the raw dataProvider structures, so it is reusable well beyond
    // grid export.
    //
    // The variable, essential inputs are positional parameters; everything
    // else rides in the trailing `options` key/value bag (unknown or
    // inapplicable keys ignored):
    //
    //   rows   : Array | XMLList  (required) row source; Array elements may be
    //            XMLReadOnly / XML / plain objects, or an XMLList.
    //   fields : Array            (required) per-column selector, same spec as
    //            XMLReadOnly.sortKeyed: "" = the row itself, "@n" = attribute n,
    //            "n" = child element / property n.
    //   header : XML              (xlsx only; may be null) grouped/multi-row header:
    //            <h><c t="A"/><g t="G"><c t="B"/><c t="C"/></g></h>. Leaf <c> order
    //            matches `fields`; the `textAlign` attribute (left/center/right)
    //            sets the column's alignment (header + data). When null (or in CSV)
    //            the column titles come from `fields` (leading "@" stripped) as a
    //            single flat header row.
    //
    // options bag:
    //   format:String="xlsx"             "xlsx" | "csv".
    //   sheetName:String="Foglio1"
    //   compression:int=6                 ZIP/deflate level 0-9.
    //   detectTypes:Boolean=true          (xlsx) auto-detect column types.
    //   detectDates:Boolean=true          (xlsx) treat YYYY-MM-DD HH:MM:SS as dates.
    //   typeSampleRows:int=1000           (xlsx) rows scanned to infer types (0=all).
    //   fontFamily:String="Calibri"       (xlsx) global font.
    //   fontSize:Number=11                (xlsx) global font size.
    //   headerBackgroundColor:uint        (xlsx) header cell fill, 0xRRGGBB.
    //   headerForegroundColor:uint        (xlsx) header font colour, 0xRRGGBB.
    //   rowBackgroundColors:Array         (xlsx) [c0, c1] alternating data-row fill.
    //   separator:String=";"             (csv) column separator.
    //   separatorHint:Boolean=false      (csv) prepend "sep=<separator>\r\n".
    //   forceText:Boolean=false          (csv) wrap every cell as ="<value>".
    //   compress:Boolean=false           (csv) return a ZIP with one "<sheetName>.csv".
    //   minChunkSize:int=100             (asyncExportBegin) floor on per-chunk rows.
    //
    // A thin black border is drawn around every xlsx cell (header + data).
    // CSV output is UTF-8 + BOM, CRLF; in CSV `header` is ignored.
    //
    // Not instantiable: call the static method directly. The application
    // resolves the class at runtime via getDefinitionByName("ExportUtils"),
    // since it compiles against the stock playerglobal.
    //
    // Asynchronous (chunked) export. Begin returns an opaque handle, Continue
    // processes one chunk of rows (1% of total, floored at minChunkSize=100
    // unless overridden), End flushes any remaining rows and returns the bytes,
    // Cancel discards the state without producing output. AS3 drives a Timer
    // (or setTimeout) loop between Continue calls so the runtime can repaint.
    //
    //   var ObjHandle:Object = ExportUtils.asyncExportBegin(rows, fields, header, options);
    //   var IntTotal:int     = ObjHandle.__total;
    //   function tick():void {
    //       var IntDone:int  = ExportUtils.asyncExportContinue(ObjHandle);
    //       ObjProgress.value = IntDone / IntTotal;
    //       if (IntDone < IntTotal) setTimeout(tick, 0);
    //       else saveFile(ExportUtils.asyncExportEnd(ObjHandle), "...");
    //   }
    public final class ExportUtils {
        public static native function syncExport(rows:Object, fields:Array, header:XML = null, options:Object = null):ByteArray;
        public static native function asyncExportBegin(rows:Object, fields:Array, header:XML = null, options:Object = null):Object;
        public static native function asyncExportContinue(handle:Object):int;
        public static native function asyncExportEnd(handle:Object):ByteArray;
        public static native function asyncExportCancel(handle:Object):void;
    }
}
