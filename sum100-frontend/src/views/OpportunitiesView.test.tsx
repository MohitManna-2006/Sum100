import { fireEvent, render, screen } from '@testing-library/react'
import { describe, expect, it } from 'vitest'
import { mockOpportunities } from '../mocks/data'
import { OpportunitiesView } from './OpportunitiesView'

describe('OpportunitiesView', () => {
  it('preserves the selected sort across live prop updates', () => {
    const { rerender } = render(
      <OpportunitiesView opportunities={mockOpportunities.slice(0, 3)} />,
    )

    fireEvent.click(screen.getByRole('button', { name: 'By edge' }))
    expect(
      screen.getByRole('button', { name: 'By edge' }).getAttribute('aria-pressed'),
    ).toBe('true')

    rerender(
      <OpportunitiesView opportunities={[...mockOpportunities.slice(0, 4)]} />,
    )
    expect(
      screen.getByRole('button', { name: 'By edge' }).getAttribute('aria-pressed'),
    ).toBe('true')
  })
})
