import { useMemo, useState } from 'react'
import type { Opportunity, SignalStatus, SortKey } from '../api/types'
import { Modal } from '../components/Modal'
import { SignalCard } from '../components/SignalCard'
import { SignalTable } from '../components/SignalTable'
import styles from './OpportunitiesView.module.css'

interface OpportunitiesViewProps {
  opportunities: Opportunity[]
}

const PAGE_SIZE = 7

export function OpportunitiesView({ opportunities }: OpportunitiesViewProps) {
  const [search, setSearch] = useState('')
  const [status, setStatus] = useState<SignalStatus | 'all'>('all')
  const [sort, setSort] = useState<SortKey>('return')
  const [page, setPage] = useState(0)
  const [selected, setSelected] = useState<Opportunity | null>(null)

  const filtered = useMemo(() => {
    const query = search.trim().toLowerCase()
    return opportunities
      .filter(
        (signal) =>
          (status === 'all' || signal.status === status) &&
          (!query ||
            signal.pair.toLowerCase().includes(query) ||
            signal.event.toLowerCase().includes(query)),
      )
      .sort((a, b) => {
        if (sort === 'edge') return b.edge - a.edge
        if (sort === 'days') return a.daysToResolution - b.daysToResolution
        return b.annualizedReturn - a.annualizedReturn
      })
  }, [opportunities, search, sort, status])

  const pageCount = Math.max(1, Math.ceil(filtered.length / PAGE_SIZE))
  const safePage = Math.min(page, pageCount - 1)
  const pageItems = filtered.slice(safePage * PAGE_SIZE, (safePage + 1) * PAGE_SIZE)
  const firstResult = filtered.length === 0 ? 0 : safePage * PAGE_SIZE + 1
  const lastResult = Math.min((safePage + 1) * PAGE_SIZE, filtered.length)
  const acceptedCount = opportunities.filter((signal) => signal.status === 'accepted').length

  const updateSort = (nextSort: SortKey) => {
    setSort(nextSort)
    setPage(0)
  }

  return (
    <section className={styles.view}>
      <div className={styles.summary}>
        <div>
          <h2 className={styles.title}>Executable opportunities</h2>
          <p className={styles.subtitle}>
            Cross-venue signals that passed book, fee, and coherence checks.
          </p>
        </div>
        <span className={styles.liveCount}>{acceptedCount} accepted now</span>
      </div>

      <div className={styles.toolbar}>
        <div className={styles.filters}>
          <input
            className={styles.search}
            type="search"
            value={search}
            onChange={(event) => {
              setSearch(event.target.value)
              setPage(0)
            }}
            placeholder="Search pair or event"
            aria-label="Search opportunities"
          />
          <select
            className={styles.select}
            value={status}
            onChange={(event) => {
              setStatus(event.target.value as SignalStatus | 'all')
              setPage(0)
            }}
            aria-label="Filter by status"
          >
            <option value="all">All statuses</option>
            <option value="accepted">Accepted</option>
            <option value="pending">Pending</option>
            <option value="rejected">Rejected</option>
          </select>
        </div>

        <div className={styles.sortGroup} aria-label="Sort opportunities">
          {(
            [
              ['return', 'By return'],
              ['edge', 'By edge'],
              ['days', 'By days'],
            ] as const
          ).map(([key, label]) => (
            <button
              className={`${styles.sortButton} ${sort === key ? styles.active : ''}`}
              type="button"
              key={key}
              onClick={() => updateSort(key)}
              aria-pressed={sort === key}
            >
              {label}
            </button>
          ))}
        </div>
      </div>

      <SignalTable
        opportunities={pageItems}
        sort={sort}
        onSort={updateSort}
        onInspect={setSelected}
      />

      <div className={styles.pagination}>
        <span>
          Showing {firstResult}–{lastResult} of {filtered.length}
        </span>
        <div className={styles.pageButtons}>
          <button
            className={styles.pageButton}
            type="button"
            disabled={safePage === 0}
            onClick={() => setPage((current) => Math.max(0, current - 1))}
          >
            Previous
          </button>
          <button
            className={styles.pageButton}
            type="button"
            disabled={safePage >= pageCount - 1}
            onClick={() => setPage((current) => Math.min(pageCount - 1, current + 1))}
          >
            Next
          </button>
        </div>
      </div>

      {selected ? (
        <Modal title={selected.pair} onClose={() => setSelected(null)}>
          <SignalCard signal={selected} />
        </Modal>
      ) : null}
    </section>
  )
}
