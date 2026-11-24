use super::*;

/// Finds the part `part_id` in a message's part tree.
pub(super) fn find_part<'a>(part: &'a gmail::PartDto, part_id: &str) -> Option<&'a gmail::PartDto> {
    if part.part_id == part_id {
        return Some(part);
    }
    part.parts.iter().find_map(|p| find_part(p, part_id))
}

/// Gathers a message's attachments within the 25 MB budget, in order: a Gmail-held one (a
/// forward's) through `gmail`, a local one read from disk. Nothing has been uploaded when this
/// fails, so every error is `maybe_sent: false`.
pub(super) fn gather_attachments(
    out: &Outgoing,
    mut gmail: impl FnMut(&str, &str) -> Result<Vec<u8>, String>,
) -> Result<Vec<(OutgoingAttachment, Vec<u8>)>, SendError> {
    let refuse = |message: String| SendError {
        message,
        maybe_sent: false,
    };
    let mut budget = MAX_OUTGOING_ATTACHMENTS;
    let mut parts = Vec::with_capacity(out.attachments.len());
    for a in &out.attachments {
        let bytes = match &a.source {
            // The declared size is close to the decoded one: refuse before downloading.
            AttachmentSource::Gmail { .. } if a.size > budget => {
                return Err(refuse(compose::TOO_LARGE_TO_SEND.into()));
            }
            AttachmentSource::Gmail {
                message_id,
                part_id,
            } => gmail(message_id, part_id).map_err(|e| {
                refuse(format!(
                    "Couldn't attach {}: {e}",
                    files::sanitise(&a.filename)
                ))
            })?,
            AttachmentSource::Local(path) => compose::read_local(path, budget).map_err(refuse)?,
        };
        budget = budget
            .checked_sub(bytes.len() as u64)
            .ok_or_else(|| refuse(compose::TOO_LARGE_TO_SEND.into()))?;
        let mut a = a.clone();
        a.size = bytes.len() as u64;
        parts.push((a, bytes));
    }
    Ok(parts)
}
