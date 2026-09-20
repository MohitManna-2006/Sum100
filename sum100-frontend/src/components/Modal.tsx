import { useEffect, useRef, type ReactNode } from 'react'
import styles from './Modal.module.css'

interface ModalProps {
  title: string
  children: ReactNode
  onClose: () => void
}

export function Modal({ title, children, onClose }: ModalProps) {
  const closeButtonRef = useRef<HTMLButtonElement>(null)

  useEffect(() => {
    const previousOverflow = document.body.style.overflow
    document.body.style.overflow = 'hidden'
    closeButtonRef.current?.focus()

    const handleKeyDown = (event: KeyboardEvent) => {
      if (event.key === 'Escape') {
        onClose()
      }
    }

    window.addEventListener('keydown', handleKeyDown)
    return () => {
      document.body.style.overflow = previousOverflow
      window.removeEventListener('keydown', handleKeyDown)
    }
  }, [onClose])

  return (
    <div
      className={styles.backdrop}
      onMouseDown={(event) => {
        if (event.currentTarget === event.target) {
          onClose()
        }
      }}
    >
      <section
        className={styles.dialog}
        role="dialog"
        aria-modal="true"
        aria-labelledby="modal-title"
      >
        <header className={styles.header}>
          <h2 className={styles.title} id="modal-title">
            {title}
          </h2>
          <button
            className={styles.close}
            type="button"
            onClick={onClose}
            ref={closeButtonRef}
            aria-label="Close dialog"
          >
            ×
          </button>
        </header>
        <div className={styles.body}>{children}</div>
      </section>
    </div>
  )
}
