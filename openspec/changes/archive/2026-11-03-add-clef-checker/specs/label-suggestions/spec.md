## MODIFIED Requirements

### Requirement: Label guidance
Settings SHALL offer a "LABEL GUIDANCE" field, up to 500 characters, saved on the device with
Save guidance and captioned "Saved on this device and sent to <processors> with label
suggestions. Avoid secrets and private information.", where the processors are "OpenRouter
and TypeSafe" for Jev and "Cloudflare" for Clef. The guidance SHALL be sent, with control
characters stripped and without redaction, only with label questions, never with a check
that asks no labels. Changing it SHALL ask about the open message's labels again.

#### Scenario: Guidance only with labels
- **WHEN** guidance is saved and suggestions are turned off
- **THEN** checks carry neither label names nor the guidance
