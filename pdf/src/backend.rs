use crate::error::*;
use crate::parser::Lexer;
use crate::parser::read_xref_and_trailer_at;
use crate::xref::XRef;
use crate::xref::XRefTable;
use crate::primitive::Dictionary;
use crate::object::*;
use std::ops::Deref;

use std::ops::{
    RangeFull,
    RangeFrom,
    RangeTo,
    Range,
};

pub const MAX_ID: u32 = 1_000_000;

pub trait Backend: Sized {
    fn read<T: IndexRange>(&self, range: T) -> Result<&[u8]>;
    //fn write<T: IndexRange>(&mut self, range: T) -> Result<&mut [u8]>;
    fn len(&self) -> usize;
    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Returns the offset of the beginning of the file, i.e., where the `%PDF-1.5` header is.
    /// (currently only used internally!)
    fn locate_start_offset(&self) -> Result<usize> {
        // Read from the beginning of the file, and look for the header.
        // Implementation note 13 in version 1.7 of the PDF reference says that Acrobat viewers
        // expect the header to be within the first 1KB of the file, so we do the same here.
        const HEADER: &[u8] = b"%PDF-";
        let buf = t!(self.read(..std::cmp::min(1024, self.len())));
        buf
            .windows(HEADER.len())
            .position(|window| window == HEADER)
            .ok_or_else(|| PdfError::Other{ msg: "file header is missing".to_string() })
    }

    /// Returns the value of startxref (currently only used internally!)
    fn locate_xref_offset(&self) -> Result<usize> {
        // locate the xref offset at the end of the file
        // `\nPOS\n%%EOF` where POS is the position encoded as base 10 integer.
        // u64::MAX has 20 digits + \n\n(2) + %%EOF(5) = 27 bytes max.

        let mut lexer = Lexer::new(t!(self.read(..)));
        lexer.set_pos_from_end(0);
        t!(lexer.seek_substr_back(b"startxref"));
        t!(lexer.next()).to::<usize>()
    }

    /// Used internally by File, but could also be useful for applications that want to look at the raw PDF objects.
    fn read_xref_table_and_trailer(&self, start_offset: usize, resolve: &impl Resolve) -> Result<(XRefTable, Dictionary)> {
        let xref_offset = t!(self.locate_xref_offset());
        let pos = t!(start_offset.checked_add(xref_offset).ok_or(PdfError::Invalid));
        if pos >= self.len() {
            bail!("XRef offset outside file bounds");
        }

        let mut lexer = Lexer::with_offset(t!(self.read(pos ..)), pos);
        
        let (xref_sections, trailer) = t!(read_xref_and_trailer_at(&mut lexer, resolve));
        
        let highest_id = t!(trailer.get("Size")
            .ok_or_else(|| PdfError::MissingEntry {field: "Size".into(), typ: "XRefTable"})?
            .as_u32());

        if highest_id > MAX_ID {
            bail!("too many objects");
        }
        let mut refs = XRefTable::new(highest_id as ObjNr);
        for section in xref_sections {
            refs.add_entries_from(section)?;
        }
        
        let mut prev_trailer = {
            match trailer.get("Prev") {
                Some(p) => Some(t!(p.as_usize())),
                None => None
            }
        };
        trace!("READ XREF AND TABLE");
        let mut seen = vec![];
        while let Some(prev_xref_offset) = prev_trailer {
            if seen.contains(&prev_xref_offset) {
                bail!("xref offsets loop");
            }
            seen.push(prev_xref_offset);

            let pos = t!(start_offset.checked_add(prev_xref_offset).ok_or(PdfError::Invalid));
            let mut lexer = Lexer::with_offset(t!(self.read(pos..)), pos);
            let (xref_sections, trailer) = t!(read_xref_and_trailer_at(&mut lexer, resolve));
            
            for section in xref_sections {
                refs.add_entries_from(section)?;
            }
            
            prev_trailer = {
                match trailer.get("Prev") {
                    Some(p) => {
                        let prev = t!(p.as_usize());
                        Some(prev)
                    }
                    None => None
                }
            };
        }
        Ok((refs, trailer))
    }

    fn restore_xref_table(&self) -> Result<XRefTable> {
        let start_offset = t!(self.locate_start_offset());
        println!("start_offset={start_offset}");
        let mut lexer = Lexer::new(t!(self.read(..)));
        let mut objects = Vec::new();

        // Ignore errors on purpose
        let _ = (|| -> Result<()> { loop {
            // FIXME: dirty, needs enhancement
            // count "<<" & ">>" markers ?
            // or fix original code below
            try_opt!(lexer.seek_substr("obj"));
            t!(lexer.back());
            let w2 = t!(lexer.back());
            let w1 = t!(lexer.back());
            let offset = lexer.get_pos();
            println!("w1={}", w1.to::<ObjNr>().unwrap());
            println!("w2={}", w2.to::<ObjNr>().unwrap());
            try_opt!(lexer.seek_substr("endobj"));
            // let end_pos = lexer.get_pos();

            // let offset = lexer.get_pos();
            // let w1 = t!(lexer.next());
            // let w2 = t!(lexer.next());
            // let w3 = t!(lexer.next_expect("obj"));
            // try_opt!(lexer.seek_substr("endobj"));

            objects.push((t!(w1.to::<ObjNr>()), t!(w2.to::<GenNr>()), offset));
        }})();

        dbg!(&objects);
    
        objects.sort_unstable();
        // let first_id = objects.first().map(|&(n, _, _)| n).unwrap_or(0);
        let highest_id = objects.last().map(|&(n, _, _)| n).unwrap_or(0);
        
        let mut xref = XRefTable::new(highest_id);
        let mut free_xrefs = vec![];
        // add obj 0 (must be free)
        free_xrefs.push((0, XRef::Free { next_obj_nr: 0, gen_nr: 0xffff }));
        
        let mut last_id = 0u64;
        for &(obj_nr, gen_nr, offset) in objects.iter() {
            // Prepare free entries
            for n in last_id+1..obj_nr {
                free_xrefs.push((n, XRef::Free { next_obj_nr: obj_nr, gen_nr: 0 }));
            }
            if obj_nr == last_id {
                warn!("duplicate obj_nr {}", obj_nr);
                continue;
            }
            xref.set(obj_nr, XRef::Raw {
                pos: offset - start_offset,
                gen_nr
            });
            // dbg!(&xref);
            last_id = obj_nr;
        }

        // TODO: test
        for (i, (n, r)) in free_xrefs.iter().enumerate() {
            // find next free obj and adjust next_obj_nr if necessary
            let free_xref = if let Some((next_obj_nr, _)) = free_xrefs.get(i+1) {
                XRef::Free { next_obj_nr: *next_obj_nr, gen_nr: r.get_gen_nr() }
            } else {
                *r
            };
            xref.set(*n, free_xref);
        }

        Ok(xref)
    }

}


impl<T> Backend for T where T: Deref<Target=[u8]> { //+ DerefMut<Target=[u8]> {
    fn read<R: IndexRange>(&self, range: R) -> Result<&[u8]> {
        let r = t!(range.to_range(self.len()));
        Ok(&self[r])
    }
    /*
    fn write<R: IndexRange>(&mut self, range: R) -> Result<&mut [u8]> {
        let r = range.to_range(self.len())?;
        Ok(&mut self[r])
    }
    */
    fn len(&self) -> usize {
        (**self).len()
    }
}

/// `IndexRange` is implemented by Rust's built-in range types, produced
/// by range syntax like `..`, `a..`, `..b` or `c..d`.
pub trait IndexRange
{
    /// Start index (inclusive)
    fn start(&self) -> Option<usize>;

    /// End index (exclusive)
    fn end(&self) -> Option<usize>;

    /// `len`: the size of whatever container that is being indexed
    fn to_range(&self, len: usize) -> Result<Range<usize>> {
        match (self.start(), self.end()) {
            (None, None) => Ok(0 .. len),
            (Some(start), None) if start <= len => Ok(start .. len),
            (None, Some(end)) if end <= len => Ok(0 .. end),
            (Some(start), Some(end)) if start <= end && end <= len => Ok(start .. end),
            _ => Err(PdfError::ContentReadPastBoundary)
        }
    }
}


impl IndexRange for RangeFull {
    #[inline]
    fn start(&self) -> Option<usize> { None }
    #[inline]
    fn end(&self) -> Option<usize> { None }

}

impl IndexRange for RangeFrom<usize> {
    #[inline]
    fn start(&self) -> Option<usize> { Some(self.start) }
    #[inline]
    fn end(&self) -> Option<usize> { None }
}

impl IndexRange for RangeTo<usize> {
    #[inline]
    fn start(&self) -> Option<usize> { None }
    #[inline]
    fn end(&self) -> Option<usize> { Some(self.end) }
}

impl IndexRange for Range<usize> {
    #[inline]
    fn start(&self) -> Option<usize> { Some(self.start) }
    #[inline]
    fn end(&self) -> Option<usize> { Some(self.end) }
}
